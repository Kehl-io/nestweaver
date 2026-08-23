//! NestWeaver daemon client — connect over Unix domain socket with auto-start.

pub mod autostart;
pub mod connect;
pub mod hybrid;
pub mod progress;
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

    /// Choose a cold-start decision without consulting any stale live-binding
    /// sidecar. An explicit config remains canonical, UTF-8, and parse-valid.
    pub fn for_cold_start(explicit_config: Option<&Path>) -> Result<Self> {
        match explicit_config {
            Some(path) => validate_restart_config(path).map(Self::Configured),
            None => Ok(Self::CompiledDefaults),
        }
    }

    /// Select configuration for an automatic cold start.
    ///
    /// Explicit caller intent wins. Otherwise the last configured daemon that
    /// reached readiness is reused. Only genuine record absence selects
    /// compiled defaults; corrupt, unsafe, or now-invalid persisted intent
    /// fails closed because it controls identity and authorization.
    pub fn for_automatic_cold_start(
        db_path: &Path,
        explicit_config: Option<&Path>,
    ) -> Result<Self> {
        if let Some(path) = explicit_config {
            return validate_restart_config(path).map(Self::Configured);
        }

        let record = match nestweaver_daemon::lifecycle::read_last_successful_config(db_path) {
            Ok(record) => record,
            Err(nestweaver_daemon::lifecycle::LastSuccessfulConfigError::Absent { .. }) => {
                return Ok(Self::CompiledDefaults);
            }
            Err(error) => {
                anyhow::bail!(
                    "cannot honor persisted daemon configuration for {}: {error}. \
                     To deliberately reset this database to compiled defaults, run \
                     `nestweaver daemon --db {} start --reset`",
                    db_path.display(),
                    db_path.display()
                );
            }
        };
        let recorded = PathBuf::from(&record.config_path);
        let canonical = validate_restart_config(&recorded).with_context(|| {
            format!(
                "persisted daemon config {} for {} is no longer usable; automatic startup refuses to fall back to compiled defaults. To deliberately reset, run `nestweaver daemon --db {} start --reset`",
                recorded.display(),
                db_path.display(),
                db_path.display()
            )
        })?;
        anyhow::ensure!(
            canonical == recorded,
            "persisted daemon config path is not canonical: {}. To deliberately reset, run `nestweaver daemon --db {} start --reset`",
            recorded.display(),
            db_path.display()
        );
        Ok(Self::Configured(canonical))
    }
}

#[derive(Debug)]
pub struct PreparedRestart {
    config: RestartConfig,
    daemon_pid: u32,
    /// The database whose write lock the replacement must be able to acquire.
    /// Owner release waits only while this lock is `Held` — see
    /// [`PreparedRestart::wait_for_owner_release`].
    db_path: PathBuf,
    /// The exact inode whose held flock and contents were cross-checked with
    /// HealthCheck during PREPARE. COMMIT never reopens or rereads the path.
    pidfile: fs::File,
    owns_pidfile_lock: bool,
}

impl PreparedRestart {
    pub fn config(&self) -> &RestartConfig {
        &self.config
    }

    /// Wait until the previous owner is provably gone.
    ///
    /// "Gone" has exactly one definition on every restart path in this binary —
    /// the same evidence daemon startup requires: the instance pidfile flock is
    /// free (so the replacement can claim the instance) AND the database write
    /// lock is not `Held` (so the replacement can open the store; the start
    /// guard in `nestweaver-daemon/src/server.rs` hard-bails only on `Held`).
    /// Like that guard, this wait PROCEEDS on `DbWriteLock::Unknown`: the
    /// probe's inability to read the lock state is not evidence of a holder,
    /// and the replacement's own `GraphStore::open_or_create` fails safely a
    /// moment later if the lock really is held. That mirrors the one deliberate
    /// exception to "treat `Unknown` as possibly-owned", documented at the
    /// start guard — matching only `Held` — and keeps restart, auto-restart and
    /// startup on one policy. The two locks are not released together: the
    /// daemon's `_pid_guard` drops deterministically at the end of `serve()`,
    /// while the write lock lives in `Arc<GraphStore>` clones that drop later
    /// and nondeterministically — measured ~0.05–0.1s behind the pidfile
    /// release on a clean teardown, and seconds when clones linger (observed
    /// live during a drain). The pidfile lock alone is therefore a strictly
    /// weaker signal, and gating on it let a restart spawn a replacement that
    /// could not open the database, leaving the database with no daemon.
    ///
    /// Caller precondition: invoke this only after the incumbent has accepted a
    /// shutdown. Both restart paths — manual `daemon restart` and the
    /// version-mismatch auto-restart — call it from the post-shutdown COMMIT
    /// step, and the phase-2 timeout message's "the incumbent was stopped"
    /// relies on that ordering; it is not a property this wait establishes.
    ///
    /// The downstream health wait deliberately still treats a dead child as
    /// terminal rather than re-spawning once on a write-lock conflict: after
    /// this gate the only remaining conflict source is a third party outside
    /// the spawn lock (e.g. a `--no-daemon` run) that holds the lock for its
    /// whole lifetime, so an immediate re-spawn would hit the same lock — and
    /// a dead child's exit status cannot distinguish a lock conflict from a
    /// genuine boot failure.
    pub async fn wait_for_owner_release(&mut self) -> Result<()> {
        let ceiling = nestweaver_schema::drain_ceiling_from_env();
        self.wait_for_owner_release_for(std::time::Duration::from_secs(ceiling.saturating_add(5)))
            .await
    }

    async fn wait_for_owner_release_for(&mut self, timeout: std::time::Duration) -> Result<()> {
        // Phase 1: the pidfile flock, released deterministically at the end of
        // the old owner's serve().
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if try_acquire_pidfile_lock(&self.pidfile)? {
                self.owns_pidfile_lock = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if !self.owns_pidfile_lock {
            anyhow::bail!(
                "daemon PID {} did not release its captured pidfile lock within {:.1}s; refusing to signal an uncertain PID or spawn a replacement. Stop the daemon manually, verify its identity, and retry",
                self.daemon_pid,
                timeout.as_secs_f64()
            );
        }

        // Phase 2: the database write lock — the evidence daemon startup
        // actually requires. It outlives the pidfile flock by the lifetime of
        // the last `Arc<GraphStore>` clone, a delay bounded by process
        // teardown rather than by the drain, so it gets its own full budget.
        // Only `Held` blocks: `Unknown` proceeds immediately, exactly as the
        // start guard does (the replacement's own DB open is the backstop), so
        // a platform where the probe cannot read the lock state does not wait
        // out the whole budget for nothing.
        use nestweaver_daemon::lifecycle::DbWriteLock;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let state = nestweaver_daemon::lifecycle::db_write_lock(&self.db_path);
            match state {
                DbWriteLock::Held { .. } => {}
                DbWriteLock::Free | DbWriteLock::Unknown => return Ok(()),
            }
            if std::time::Instant::now() >= deadline {
                let detail = match state {
                    DbWriteLock::Held { pid: Some(pid) } => format!("still held by PID {pid}"),
                    _ => "still held by another process".to_string(),
                };
                anyhow::bail!(
                    "daemon PID {} released its pidfile lock, but {:.1}s later the write lock on {} is {detail}. \
                     The incumbent was stopped and no replacement was started — the database currently has no daemon. \
                     Once the holder exits, bring one up with `nestweaver daemon --db {} start`",
                    self.daemon_pid,
                    timeout.as_secs_f64(),
                    self.db_path.display(),
                    self.db_path.display()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
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
        .open(path)
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

/// The identity gate for an incumbent daemon that answered a HealthCheck:
/// the pidfile-flock proof first, and — when that proof can no longer run
/// because the pidfile was unlinked under a live daemon (its flock survives
/// on an inode no path reaches) — corroboration by kernel-supplied socket
/// peer credentials. Returns the opened pidfile when the flock gate ran,
/// `None` when identity was established by peer credentials instead.
///
/// On refusal the error is the ORIGINAL pidfile-gate error with the
/// peer-credential detail appended, so today's diagnostic string survives.
fn verified_incumbent_identity(
    db_path: &Path,
    instance_id: &str,
    health: &nestweaver_proto::HealthCheckResponse,
) -> Result<Option<fs::File>> {
    match open_verified_live_pidfile(instance_id, health.pid) {
        Ok(file) => Ok(Some(file)),
        Err(pidfile_error) => {
            adopt_pidfileless_incumbent(db_path, instance_id, health).map_err(|refusal| {
                pidfile_error.context(format!(
                    "socket peer-credential corroboration also failed: {refusal}"
                ))
            })?;
            Ok(None)
        }
    }
}

/// Corroborate the HealthCheck identity of a daemon whose pidfile flock can
/// no longer be verified — the signature state of `rm daemon.pid` under a
/// live daemon. This gates ADOPTION of the incumbent for reads; it is an
/// anti-impersonation control, so every check must hold, and all of the
/// evidence is kernel-supplied — a process that merely created files in the
/// runtime dir cannot forge any of it:
///
///  1. `health.instance_id` must match the requested instance — enforced by
///     `verify_replacement_evidence` immediately after this returns, against
///     the same health snapshot.
///  2. The kernel-reported PID of the socket peer must equal `health.pid`
///     (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS).
///  3. On platforms reporting a peer uid (Linux), it must equal our euid —
///     a different user's process cannot pose as our daemon.
///  4. The database write lock must not contradict the identity — see
///     [`adoption_lock_verdict`].
///
/// The `Err` payload is a human-readable refusal reason the caller appends
/// to the original pidfile-gate error.
fn adopt_pidfileless_incumbent(
    db_path: &Path,
    instance_id: &str,
    health: &nestweaver_proto::HealthCheckResponse,
) -> std::result::Result<(), String> {
    let socket = nestweaver_daemon::lifecycle::socket_path(instance_id);
    let stream = std::os::unix::net::UnixStream::connect(&socket).map_err(|error| {
        format!(
            "cannot connect to daemon socket {}: {error}",
            socket.display()
        )
    })?;
    socket_peer_matches_health(&stream, health.pid)?;
    adoption_lock_verdict(
        nestweaver_daemon::lifecycle::db_write_lock(db_path),
        health.pid,
    )?;
    info!(
        pid = health.pid,
        "adopting the incumbent daemon whose pidfile was unlinked: \
         socket peer credentials corroborate the HealthCheck identity"
    );
    Ok(())
}

/// Checks 2 (and 3 on Linux) of [`adopt_pidfileless_incumbent`]: the
/// kernel-reported socket peer identity must match the HealthCheck PID, and
/// where the platform reports a peer uid it must match our euid. A platform
/// that cannot name the peer cannot corroborate and refuses.
#[cfg(target_os = "linux")]
fn socket_peer_matches_health(
    stream: &std::os::unix::net::UnixStream,
    health_pid: u32,
) -> std::result::Result<(), String> {
    let Some((pid, uid)) = nestweaver_daemon::lifecycle::unix_socket_peer_cred(stream) else {
        return Err("kernel reported no peer credentials for the daemon socket".to_string());
    };
    if pid != health_pid as i32 {
        return Err(format!(
            "socket peer PID {pid} does not match HealthCheck PID {health_pid}"
        ));
    }
    let euid = unsafe { libc::geteuid() };
    if uid != euid {
        return Err(format!(
            "socket peer uid {uid} does not match the client euid {euid}"
        ));
    }
    Ok(())
}

/// Non-Linux platforms report a peer PID but no uid, so only check 2 runs.
#[cfg(not(target_os = "linux"))]
fn socket_peer_matches_health(
    stream: &std::os::unix::net::UnixStream,
    health_pid: u32,
) -> std::result::Result<(), String> {
    match nestweaver_daemon::lifecycle::unix_socket_peer_pid(stream) {
        Some(pid) if pid == health_pid as i32 => Ok(()),
        Some(pid) => Err(format!(
            "socket peer PID {pid} does not match HealthCheck PID {health_pid}"
        )),
        None => Err("kernel reported no peer PID for the daemon socket".to_string()),
    }
}

/// Check 4 of [`adoption_lock_verdict`](adopt_pidfileless_incumbent): the
/// database write lock — the one ownership proof an operator's `rm` cannot
/// erase — must not contradict the HealthCheck identity.
///
/// `Held` by a DIFFERENT pid refuses: someone else owns the database, so the
/// socket peer is not its writer. An anonymous holder and an unreadable lock
/// state (`Unknown`) also refuse — adoption has no net: unlike daemon
/// startup's proceed-on-`Unknown` exception (whose own DB open is the
/// backstop and fails safely), nothing downstream of adoption fails safely,
/// so it must fail closed. `Free` accepts (a read-only snapshot replica
/// never takes the write lock), and `Held` by the HealthCheck PID itself is
/// the strongest case.
fn adoption_lock_verdict(
    lock: nestweaver_daemon::lifecycle::DbWriteLock,
    health_pid: u32,
) -> std::result::Result<(), String> {
    use nestweaver_daemon::lifecycle::DbWriteLock;
    match lock {
        DbWriteLock::Held { pid: Some(holder) } if holder == health_pid as i32 => Ok(()),
        DbWriteLock::Held { pid: Some(holder) } => Err(format!(
            "database write lock is held by PID {holder}, not the HealthCheck PID {health_pid}"
        )),
        DbWriteLock::Held { pid: None } => {
            Err("database write lock is held by a process the kernel does not name".to_string())
        }
        DbWriteLock::Unknown => Err(
            "database write lock state is unreadable; adoption cannot rule out a different owner"
                .to_string(),
        ),
        DbWriteLock::Free => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartRequest {
    AutomaticReplacement,
    ExplicitCommand,
}

fn restart_ownership_allowed(
    owner: nestweaver_daemon::lifecycle::DaemonLifecycleOwner,
    request: RestartRequest,
    verified_platform_supervisor: bool,
) -> bool {
    owner == nestweaver_daemon::lifecycle::DaemonLifecycleOwner::NestweaverManaged
        || (request == RestartRequest::ExplicitCommand && verified_platform_supervisor)
}

#[cfg(target_os = "macos")]
fn launchd_supervisor_matches(db_path: &Path, instance_id: &str) -> bool {
    if !nestweaver_daemon::launchd::is_running(instance_id) {
        return false;
    }
    let plist = nestweaver_daemon::lifecycle::launchd_plist_path(instance_id);
    let Ok(contents) = std::fs::read_to_string(plist) else {
        return false;
    };
    let Some(plist_db) = nestweaver_daemon::launchd::parse_db_path_from_plist(&contents) else {
        return false;
    };
    nestweaver_daemon::lifecycle::canonical_db_path(&plist_db)
        == nestweaver_daemon::lifecycle::canonical_db_path(db_path)
}

#[cfg(not(target_os = "macos"))]
fn launchd_supervisor_matches(_db_path: &Path, _instance_id: &str) -> bool {
    false
}

/// Capture trustworthy live daemon ownership and configuration before any
/// automatic version-replacement shutdown request. Callers must not shut down
/// when this returns an error.
pub fn prepare_restart(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    explicit_config: Option<&Path>,
) -> Result<PreparedRestart> {
    prepare_restart_for(
        db_path,
        health,
        explicit_config,
        RestartRequest::AutomaticReplacement,
    )
}

/// Prepare a user-requested `daemon restart`.
///
/// Automatic replacement remains limited to NestWeaver-managed detached
/// daemons. An explicit command may also restart a daemon whose launchd job and
/// plist are both verified to target this exact database; the restart command
/// preserves that supervisor route instead of replacing it with a detached
/// process.
pub fn prepare_explicit_restart(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    explicit_config: Option<&Path>,
) -> Result<PreparedRestart> {
    prepare_restart_for(
        db_path,
        health,
        explicit_config,
        RestartRequest::ExplicitCommand,
    )
}

fn prepare_restart_for(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    explicit_config: Option<&Path>,
    request: RestartRequest,
) -> Result<PreparedRestart> {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let prepared = (|| {
        anyhow::ensure!(
            health.instance_id == instance_id,
            "daemon HealthCheck instance {} does not match requested instance {instance_id}",
            health.instance_id
        );
        let pidfile = open_verified_live_pidfile(&instance_id, health.pid)?;
        let binding = nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
            &instance_id,
            health.pid,
        )?;
        let verified_platform_supervisor = launchd_supervisor_matches(db_path, &instance_id);
        anyhow::ensure!(
            restart_ownership_allowed(
                binding.lifecycle_owner,
                request,
                verified_platform_supervisor
            ),
            "daemon PID {} is supervisor-managed, foreground, or has unknown lifecycle ownership; refusing to shut it down and replace it with a detached process. Restart it through its supervisor (for systemd: `systemctl --user restart nestweaver`; for launchd: `launchctl kickstart -k gui/$(id -u)/{}`), or stop it and run `nestweaver daemon --db {} start`",
            health.pid,
            nestweaver_daemon::lifecycle::launchd_label(&instance_id),
            db_path.display()
        );
        let config = select_restart_config(explicit_config, || Ok(binding))?;
        Ok(PreparedRestart {
            config,
            daemon_pid: health.pid,
            db_path: db_path.to_path_buf(),
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
             The daemon has not been shut down. Restart it through its supervisor. If it is intentionally \
             NestWeaver-managed, run `nestweaver daemon --db {} stop`, verify that it exits, then run \
             `nestweaver daemon --db {} start --config <path>`",
            db_path.display(),
            db_path.display()
        )
    })
}

/// Transaction gate shared by production and ordering tests. Neither callback
/// runs when PREPARE fails, and COMMIT cannot run unless Shutdown was accepted.
pub async fn run_prepared_restart<T, Shutdown, ShutdownFuture, Commit, CommitFuture>(
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

fn restart_with_requested_config_remedy(db_path: &Path, requested: &RestartConfig) -> String {
    let requested = requested
        .as_path()
        .expect("explicit requested config is always configured");
    format!(
        "Run `nestweaver daemon --db {} restart --config {}` to apply the requested configuration",
        db_path.display(),
        requested.display()
    )
}

fn verify_requested_config_evidence(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    requested: &RestartConfig,
    binding: nestweaver_daemon::lifecycle::EffectiveConfigBinding,
) -> Result<()> {
    let expected_instance = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    anyhow::ensure!(
        health.instance_id == expected_instance,
        "running daemon instance {} does not match requested DB instance {expected_instance}; {}",
        health.instance_id,
        restart_with_requested_config_remedy(db_path, requested)
    );
    anyhow::ensure!(health.pid != 0, "running daemon HealthCheck returned PID 0");
    anyhow::ensure!(
        binding.pid == health.pid,
        "running daemon binding PID {} does not match HealthCheck PID {}; {}",
        binding.pid,
        health.pid,
        restart_with_requested_config_remedy(db_path, requested)
    );
    let effective = select_restart_config(None, || Ok(binding)).with_context(|| {
        format!(
            "cannot verify the running daemon's effective config for explicit --config {}; effective config is unknown. {}",
            requested.as_path().unwrap().display(),
            restart_with_requested_config_remedy(db_path, requested)
        )
    })?;
    if &effective != requested {
        let effective_description = match &effective {
            RestartConfig::Configured(path) => path.display().to_string(),
            RestartConfig::CompiledDefaults => "compiled defaults".to_string(),
        };
        anyhow::bail!(
            "explicit --config {} does not match the running daemon's effective config ({effective_description}). {}",
            requested.as_path().unwrap().display(),
            restart_with_requested_config_remedy(db_path, requested)
        );
    }
    Ok(())
}

fn verify_requested_config_with_health(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    requested_path: &Path,
) -> Result<()> {
    let requested = RestartConfig::for_cold_start(Some(requested_path))?;
    let remedy = restart_with_requested_config_remedy(db_path, &requested);
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let _pidfile = open_verified_live_pidfile(&instance_id, health.pid).with_context(|| {
        format!(
            "cannot prove ownership for the running daemon while enforcing explicit --config {}. {}",
            requested.as_path().unwrap().display(),
            restart_with_requested_config_remedy(db_path, &requested)
        )
    })?;
    let binding = nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
        &instance_id,
        health.pid,
    )
    .with_context(|| {
        format!(
            "cannot verify the running daemon's effective config for explicit --config {}; effective config is unknown. {}",
            requested.as_path().unwrap().display(),
            restart_with_requested_config_remedy(db_path, &requested)
        )
    })?;
    verify_requested_config_evidence(db_path, health, &requested, binding).with_context(|| {
        format!(
            "the running daemon did not accept explicit --config {}; {remedy}",
            requested.as_path().unwrap().display()
        )
    })
}

/// Require an already-running daemon to prove that it is honoring an explicit
/// caller config. Canonical path identity is the contract; valid edits at the
/// same path do not require a restart.
pub async fn verify_running_daemon_config(db_path: &Path, requested_path: &Path) -> Result<()> {
    let requested = RestartConfig::for_cold_start(Some(requested_path))?;
    let remedy = restart_with_requested_config_remedy(db_path, &requested);
    let mut client = DaemonClient::connect_existing(db_path)
        .await
        .with_context(|| {
            format!(
                "cannot reach a healthy running daemon to verify explicit --config {}; effective config is unknown. {remedy}",
                requested.as_path().unwrap().display()
            )
        })?;
    let health = client.health_check().await.with_context(|| {
        format!(
            "cannot verify HealthCheck for explicit --config {}; effective config is unknown. {remedy}",
            requested.as_path().unwrap().display()
        )
    })?;
    verify_requested_config_with_health(db_path, &health, requested_path)
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
        let sock_path = autostart::ensure_daemon_for_client_async(db_path, config_path).await?;
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

                    // wait_for_owner_release returned, so the old owner's
                    // pidfile flock is released AND the database write lock is
                    // not Held — the same evidence daemon startup requires
                    // (Unknown proceeds, exactly as the start guard's
                    // deliberate exception does). We hold the inode lock, and
                    // a stale socket cannot belong to a successor because the
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
        } else if let Some(requested_config) = config_path {
            // A current-version daemon is not restarted, so success is allowed
            // only after proving that the explicit caller config is the same
            // canonical path as the daemon's typed live provenance.
            verify_requested_config_with_health(db_path, &resp, requested_config)?;
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

    /// List Contract nodes through the daemon, optionally resolving an exact
    /// repository UID or case-insensitive repository display name there.
    pub async fn list_contracts(
        &mut self,
        repo: Option<&str>,
    ) -> Result<Vec<nestweaver_schema::Contract>> {
        let response = self
            .inner
            .list_contracts(nestweaver_proto::ListContractsRequest {
                repo: repo.map(str::to_string),
            })
            .await
            .context("list_contracts RPC failed")?
            .into_inner();
        Ok(response
            .contracts
            .into_iter()
            .map(|contract| nestweaver_schema::Contract {
                uid: contract.uid,
                kind: contract.kind,
                verb: contract.verb,
                path: contract.path,
                operation_id: contract.operation_id,
                repo_uid: contract.repo_uid,
                source_path: contract.source_path,
                confidence: contract.confidence,
            })
            .collect())
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

    /// Wait for a just-spawned daemon using one end-to-end deadline budget.
    ///
    /// Connection establishment, gRPC health, pidfile ownership, and live
    /// effective-config attestation all fit inside `timeout`. `ignore_pid` is
    /// the unowned pidfile value observed before spawn and must not be mistaken
    /// for the replacement dying before it has rewritten the pidfile.
    ///
    /// When the pidfile flock proof cannot run — the pidfile was unlinked
    /// under a live daemon, so its lock survives only on the orphaned inode —
    /// the incumbent is ADOPTED for reads on kernel-supplied socket peer
    /// credentials instead (see [`verified_incumbent_identity`]). This scope
    /// cut is deliberate: the restart/signaling paths ([`prepare_restart`],
    /// [`connect_verified_replacement`], [`PreparedRestart::wait_for_owner_release`])
    /// KEEP the strict pidfile-inode gate, so a pidfile-less daemon can be
    /// adopted for reads but never auto-restarted or signaled.
    pub async fn wait_ready(
        db_path: &Path,
        timeout: std::time::Duration,
        ignore_pid: Option<i32>,
        expected_config: &RestartConfig,
    ) -> Result<nestweaver_proto::HealthCheckResponse> {
        let started = std::time::Instant::now();
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
        let mut delay = std::time::Duration::from_millis(50);
        let max_delay = std::time::Duration::from_millis(500);

        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, Self::connect_existing(db_path)).await {
                Ok(Ok(mut client)) => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    match tokio::time::timeout(remaining, client.health_check()).await {
                        Ok(Ok(health)) => {
                            // Health is the externally visible readiness
                            // boundary. By contract, durable configured intent
                            // and the live binding are already published.
                            if started.elapsed() >= timeout {
                                break;
                            }
                            let _owned =
                                verified_incumbent_identity(db_path, &instance_id, &health)?;
                            let binding = nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
                                &instance_id,
                                health.pid,
                            )?;
                            verify_replacement_evidence(
                                &instance_id,
                                &health,
                                expected_config,
                                binding,
                            )?;
                            if started.elapsed() >= timeout {
                                break;
                            }
                            return Ok(health);
                        }
                        Ok(Err(error)) => tracing::debug!(
                            "wait_ready: daemon connected but health is not ready: {error:#}"
                        ),
                        Err(_) => break,
                    }
                }
                Ok(Err(error)) => {
                    tracing::debug!("wait_ready: daemon not connectable yet: {error:#}")
                }
                Err(_) => break,
            }

            if let Some(pid) = autostart::read_pid(&pidfile)
                && Some(pid) != ignore_pid
                && !autostart::is_process_alive(pid)
            {
                anyhow::bail!(
                    "daemon process {pid} exited before becoming healthy for {}. Check the daemon logs: {}. \
                     If this was a restart replacement (e.g. it lost the database write lock to a third party \
                     after the previous owner released it), the previous daemon is already stopped and the \
                     database currently has no daemon — `nestweaver daemon --db {} start` brings one up",
                    db_path.display(),
                    nestweaver_daemon::lifecycle::log_hint(&instance_id),
                    db_path.display()
                );
            }

            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(delay.min(remaining)).await;
            delay = (delay * 2).min(max_delay);
        }

        anyhow::bail!(
            "daemon for {} did not become healthy and attest its effective configuration within {:.1}s. Check the daemon logs: {}. If startup is simply slow, raise {}",
            db_path.display(),
            timeout.as_secs_f64(),
            nestweaver_daemon::lifecycle::log_hint(&instance_id),
            autostart::DAEMON_BOOT_TIMEOUT_ENV
        )
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

    /// Reclaim embedding vectors left behind by deleted graph nodes (nw-204).
    ///
    /// `dry_run` reports occupancy without writing. Note that a dry run can
    /// only see what is ALREADY tombstoned; orphans that were never tombstoned
    /// are discovered by the reconcile a real run performs, so a dry run's
    /// count is a floor rather than a total.
    pub async fn compact_embeddings(
        &mut self,
        dry_run: bool,
    ) -> Result<nestweaver_proto::CompactEmbeddingsResponse> {
        let resp = self
            .inner
            .compact_embeddings(nestweaver_proto::CompactEmbeddingsRequest { dry_run })
            .await
            .context("compact_embeddings RPC failed")?;
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
    fn cold_restart_ignores_stale_provenance_and_uses_explicit_or_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = valid_config(&dir, "cold-explicit.toml");
        let canonical = fs::canonicalize(&explicit).unwrap();

        assert_eq!(
            RestartConfig::for_cold_start(None).unwrap(),
            RestartConfig::CompiledDefaults
        );
        assert_eq!(
            RestartConfig::for_cold_start(Some(&explicit)).unwrap(),
            RestartConfig::Configured(canonical)
        );
        let missing = dir.path().join("stale-sidecar-config.toml");
        assert!(
            RestartConfig::for_cold_start(Some(&missing)).is_err(),
            "an explicit cold config is validated even though stale sidecar provenance is ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_config_identity_is_canonical_for_relative_and_symlink_paths() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir_in(".").unwrap();
        let real = valid_config(&dir, "real.toml");
        let link = dir.path().join("alias.toml");
        symlink(fs::canonicalize(&real).unwrap(), &link).unwrap();
        let relative = RestartConfig::for_cold_start(Some(&real)).unwrap();
        let aliased = RestartConfig::for_cold_start(Some(&link)).unwrap();
        assert_eq!(relative, aliased);
        assert!(relative.as_path().unwrap().is_absolute());
    }

    #[test]
    fn explicit_config_evidence_names_requested_and_effective_mismatch_states() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let requested_path = fs::canonicalize(valid_config(&dir, "requested.toml")).unwrap();
        let effective_path = fs::canonicalize(valid_config(&dir, "effective.toml")).unwrap();
        let requested = RestartConfig::Configured(requested_path.clone());
        let health = nestweaver_proto::HealthCheckResponse {
            instance_id: nestweaver_daemon::lifecycle::instance_id_from_db_path(&db),
            pid: 91,
            ..Default::default()
        };

        let different = verify_requested_config_evidence(
            &db,
            &health,
            &requested,
            binding(
                91,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: effective_path.to_str().unwrap().to_string(),
                },
            ),
        )
        .unwrap_err();
        let message = format!("{different:#}");
        assert!(
            message.contains(requested_path.to_str().unwrap()),
            "{message}"
        );
        assert!(
            message.contains(effective_path.to_str().unwrap()),
            "{message}"
        );
        assert!(message.contains("daemon --db"), "{message}");
        assert!(message.contains("restart --config"), "{message}");

        let defaults = verify_requested_config_evidence(
            &db,
            &health,
            &requested,
            binding(
                91,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
            ),
        )
        .unwrap_err();
        assert!(format!("{defaults:#}").contains("compiled defaults"));

        // Editing valid contents at the SAME canonical path does not change
        // identity; path equality remains sufficient.
        fs::write(
            &requested_path,
            fs::read_to_string(&requested_path)
                .unwrap()
                .replace("restart-test", "restart-test-edited"),
        )
        .unwrap();
        verify_requested_config_evidence(
            &db,
            &health,
            &requested,
            binding(
                91,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: requested_path.to_str().unwrap().to_string(),
                },
            ),
        )
        .unwrap();
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
        assert!(
            format!("{old_daemon:#}").contains("Restart it through its supervisor"),
            "{old_daemon:#}"
        );
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
            db_path: PathBuf::from("/unused.lbug"),
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
            db_path: dir.path().join("brain.lbug"),
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

    /// Fork a child that takes a POSIX write lock (`fcntl(F_SETLK, F_WRLCK)`)
    /// on `path` and holds it until killed. A *separate process* is required:
    /// POSIX record locks never conflict with the calling process, so an
    /// in-process lock would be invisible to `F_GETLK`. The child touches only
    /// async-signal-safe libc calls after `fork` (the path CString and the pipe
    /// are prepared beforehand), which is what makes this safe from a
    /// multi-threaded test harness.
    fn fork_db_write_lock_holder(path: &Path) -> i32 {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe {
                libc::close(fds[0]);
                let fd = libc::open(c_path.as_ptr(), libc::O_RDWR);
                if fd < 0 {
                    libc::_exit(11);
                }
                let mut lock: libc::flock = std::mem::zeroed();
                lock.l_type = libc::F_WRLCK as libc::c_short;
                lock.l_whence = libc::SEEK_SET as libc::c_short;
                if libc::fcntl(fd, libc::F_SETLK, &lock) != 0 {
                    libc::_exit(12);
                }
                let ready = [1u8];
                libc::write(fds[1], ready.as_ptr() as *const libc::c_void, 1);
                loop {
                    libc::pause();
                }
            }
        }
        unsafe { libc::close(fds[1]) };
        let mut ready = [0u8; 1];
        let read = unsafe { libc::read(fds[0], ready.as_mut_ptr() as *mut libc::c_void, 1) };
        unsafe { libc::close(fds[0]) };
        assert_eq!(read, 1, "lock-holding child never signalled readiness");
        pid
    }

    fn reap(pid: i32) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }

    /// SIGKILL + reap the forked lock holder even when the test fails before
    /// its explicit reap, so a failure mode never orphans a paused child
    /// holding a POSIX lock.
    struct ChildGuard(i32);

    impl ChildGuard {
        fn reap_now(mut self) {
            reap(self.0);
            self.0 = -1;
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0 > 0 {
                reap(self.0);
            }
        }
    }

    /// The defect behind the daemonless restart: the incumbent's pidfile guard
    /// drops at the end of `serve()` while the database write lock — held by
    /// `Arc<GraphStore>` clones — is released later and nondeterministically.
    /// Owner-release evidence must therefore cover BOTH locks: the pidfile
    /// flock being free says nothing about the write lock daemon startup
    /// actually requires.
    #[tokio::test]
    async fn owner_release_with_free_pidfile_but_held_write_lock_is_not_release() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        fs::write(&pidfile, "1").unwrap();
        let observed = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pidfile)
            .unwrap();

        let db = dir.path().join("brain.lbug");
        fs::write(&db, b"placeholder").unwrap();
        let holder = ChildGuard(fork_db_write_lock_holder(&db));

        let mut prepared = PreparedRestart {
            config: RestartConfig::CompiledDefaults,
            daemon_pid: holder.0 as u32,
            db_path: db.clone(),
            pidfile: observed,
            owns_pidfile_lock: false,
        };

        let released = prepared
            .wait_for_owner_release_for(std::time::Duration::from_millis(300))
            .await;
        let error = released.expect_err(
            "pidfile lock free but database write lock still held — treating this as \
             owner release spawns a replacement that cannot open the database and \
             leaves it with no daemon",
        );
        assert!(
            format!("{error:#}").contains("write lock"),
            "unexpected error: {error:#}"
        );

        // Once the holder exits, the same wait must succeed promptly.
        holder.reap_now();
        prepared
            .wait_for_owner_release_for(std::time::Duration::from_secs(5))
            .await
            .expect("write lock released — owner release is now provable");
    }

    /// The gate mirrors daemon startup's one deliberate exception to "treat
    /// `Unknown` as possibly-owned": a lock state the probe cannot read is not
    /// evidence of a holder, so the wait proceeds immediately rather than
    /// burning its whole budget (the replacement's own DB open is the
    /// backstop, exactly as it is for the start guard).
    #[tokio::test]
    async fn owner_release_proceeds_when_write_lock_state_is_unknown() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        fs::write(&pidfile, "1").unwrap();
        let observed = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pidfile)
            .unwrap();

        let db = dir.path().join("brain.lbug");
        fs::write(&db, b"placeholder").unwrap();
        // Unreadable database file: the probe's File::open fails, which is
        // DbWriteLock::Unknown (as root the open succeeds instead and the
        // probe reports Free — the wait proceeds promptly on either).
        fs::set_permissions(&db, fs::Permissions::from_mode(0o000)).unwrap();

        let mut prepared = PreparedRestart {
            config: RestartConfig::CompiledDefaults,
            daemon_pid: std::process::id(),
            db_path: db,
            pidfile: observed,
            owns_pidfile_lock: false,
        };

        let started = std::time::Instant::now();
        prepared
            .wait_for_owner_release_for(std::time::Duration::from_secs(5))
            .await
            .expect("Unknown is not evidence of a holder — the gate must proceed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the gate must proceed immediately on Unknown, not wait out its budget"
        );
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

    // ── pidfile-less daemon adoption (verified_incumbent_identity) ────────

    fn health_for(pid: u32, instance_id: &str) -> nestweaver_proto::HealthCheckResponse {
        nestweaver_proto::HealthCheckResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            instance_id: instance_id.to_string(),
            pid,
            ..Default::default()
        }
    }

    /// A temp database with its REAL per-instance runtime dir created, so
    /// tests can bind listeners and forge pidfiles at the paths the client
    /// actually probes. Cleans up the global runtime artifacts even on panic.
    struct RuntimeDirFixture {
        instance_id: String,
        db: PathBuf,
        socket: PathBuf,
        pidfile: PathBuf,
    }

    impl RuntimeDirFixture {
        fn new(dir: &tempfile::TempDir) -> Self {
            let db = dir.path().join("brain.lbug");
            fs::write(&db, b"placeholder").unwrap();
            let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db);
            fs::create_dir_all(nestweaver_daemon::lifecycle::runtime_dir(&instance_id)).unwrap();
            Self {
                socket: nestweaver_daemon::lifecycle::socket_path(&instance_id),
                pidfile: nestweaver_daemon::lifecycle::pidfile_path(&instance_id),
                instance_id,
                db,
            }
        }
    }

    impl Drop for RuntimeDirFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.socket);
            let _ = fs::remove_file(&self.pidfile);
            let _ =
                fs::remove_dir_all(nestweaver_daemon::lifecycle::runtime_dir(&self.instance_id));
        }
    }

    #[test]
    fn version_replacement_requires_verified_nestweaver_ownership() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let fixture = RuntimeDirFixture::new(&dir);
        let pid = std::process::id();
        fs::write(&fixture.pidfile, format!("{pid}\n")).unwrap();
        let owner = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.pidfile)
            .unwrap();
        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX) }, 0);
        let health = health_for(pid, &fixture.instance_id);

        let external = nestweaver_daemon::lifecycle::EffectiveConfigBinding::new(
            pid,
            nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
        );
        nestweaver_daemon::lifecycle::write_effective_config_binding(
            &fixture.instance_id,
            &external,
        )
        .unwrap();
        let refusal = prepare_restart(&fixture.db, &health, None).unwrap_err();
        assert!(
            format!("{refusal:#}").contains("refusing to shut it down"),
            "{refusal:#}"
        );

        let autostart =
            nestweaver_daemon::lifecycle::EffectiveConfigBinding::new_with_lifecycle_owner(
                pid,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
                nestweaver_daemon::lifecycle::DaemonLifecycleOwner::NestweaverManaged,
            );
        nestweaver_daemon::lifecycle::write_effective_config_binding(
            &fixture.instance_id,
            &autostart,
        )
        .unwrap();
        let prepared = prepare_restart(&fixture.db, &health, None)
            .expect("a live, attested NestWeaver-managed owner may be replaced");
        assert_eq!(prepared.daemon_pid, pid);
    }

    #[test]
    fn only_an_explicit_command_may_use_verified_platform_supervisor_ownership() {
        use nestweaver_daemon::lifecycle::DaemonLifecycleOwner;

        assert!(restart_ownership_allowed(
            DaemonLifecycleOwner::NestweaverManaged,
            RestartRequest::AutomaticReplacement,
            false,
        ));
        assert!(!restart_ownership_allowed(
            DaemonLifecycleOwner::ExternalOrUnknown,
            RestartRequest::AutomaticReplacement,
            true,
        ));
        assert!(restart_ownership_allowed(
            DaemonLifecycleOwner::ExternalOrUnknown,
            RestartRequest::ExplicitCommand,
            true,
        ));
        assert!(!restart_ownership_allowed(
            DaemonLifecycleOwner::ExternalOrUnknown,
            RestartRequest::ExplicitCommand,
            false,
        ));
    }

    #[test]
    fn adoption_lock_verdict_refuses_contradicting_unknown_and_anonymous_locks() {
        use nestweaver_daemon::lifecycle::DbWriteLock;

        // Held by the HealthCheck PID itself: the strongest case.
        assert!(adoption_lock_verdict(DbWriteLock::Held { pid: Some(71) }, 71).is_ok());
        // Free: the read-only snapshot-replica precedent.
        assert!(adoption_lock_verdict(DbWriteLock::Free, 71).is_ok());

        let held_elsewhere =
            adoption_lock_verdict(DbWriteLock::Held { pid: Some(999) }, 71).unwrap_err();
        assert!(
            held_elsewhere.contains("held by PID 999"),
            "{held_elsewhere}"
        );
        let anonymous = adoption_lock_verdict(DbWriteLock::Held { pid: None }, 71).unwrap_err();
        assert!(anonymous.contains("does not name"), "{anonymous}");
        // Unlike daemon startup's proceed-on-Unknown exception, adoption has
        // no subsequent action that fails safely — Unknown refuses.
        let unknown = adoption_lock_verdict(DbWriteLock::Unknown, 71).unwrap_err();
        assert!(unknown.contains("unreadable"), "{unknown}");
    }

    /// A rogue listener squatting on the instance socket whose HealthCheck
    /// claims a PID the kernel does not corroborate is REFUSED — the
    /// anti-impersonation property the pidfile flock used to provide.
    #[test]
    fn adoption_refuses_a_rogue_listener_with_a_foreign_health_pid() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = RuntimeDirFixture::new(&dir);
        let _rogue = std::os::unix::net::UnixListener::bind(&fixture.socket).unwrap();

        // The socket peer is THIS process; the HealthCheck claims another PID.
        let health = health_for(std::process::id() + 1000, &fixture.instance_id);
        let refusal = adopt_pidfileless_incumbent(&fixture.db, &fixture.instance_id, &health)
            .expect_err("a peer-PID mismatch must refuse adoption");
        assert!(
            refusal.contains("does not match HealthCheck PID"),
            "{refusal}"
        );
    }

    /// An honest HealthCheck (the peer IS the claimed PID, same uid) is still
    /// refused when the database write lock names a different owner.
    #[test]
    fn adoption_refuses_when_the_write_lock_is_held_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = RuntimeDirFixture::new(&dir);
        let _listener = std::os::unix::net::UnixListener::bind(&fixture.socket).unwrap();
        let _holder = ChildGuard(fork_db_write_lock_holder(&fixture.db));

        let health = health_for(std::process::id(), &fixture.instance_id);
        let refusal = adopt_pidfileless_incumbent(&fixture.db, &fixture.instance_id, &health)
            .expect_err("a write lock held by another process must refuse adoption");
        assert!(
            refusal.contains("database write lock is held by PID"),
            "{refusal}"
        );
    }

    /// The accept case: peer PID and uid match the HealthCheck identity and
    /// the write lock does not contradict it (`Free` — the read-only replica
    /// precedent).
    #[test]
    fn adoption_accepts_the_honest_incumbent_on_peer_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = RuntimeDirFixture::new(&dir);
        let _listener = std::os::unix::net::UnixListener::bind(&fixture.socket).unwrap();

        let health = health_for(std::process::id(), &fixture.instance_id);
        adopt_pidfileless_incumbent(&fixture.db, &fixture.instance_id, &health)
            .expect("kernel-corroborated identity with a free write lock must adopt");
    }

    /// A forged pidfile whose flock is trivially free, with nothing serving
    /// the socket, is refused — and the refusal keeps today's pidfile
    /// diagnostic string with the peer-credential detail appended.
    #[test]
    fn forged_pidfile_without_a_served_socket_is_refused_with_the_pidfile_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = RuntimeDirFixture::new(&dir);
        // Forge: our own PID in the pidfile, no flock held, no socket peer.
        fs::write(&fixture.pidfile, format!("{}\n", std::process::id())).unwrap();

        let health = health_for(std::process::id(), &fixture.instance_id);
        let error = verified_incumbent_identity(&fixture.db, &fixture.instance_id, &health)
            .expect_err("a forged pidfile without a served socket must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("daemon pidfile lock is not held"),
            "the original pidfile diagnostic must survive: {message}"
        );
        assert!(
            message.contains("socket peer-credential corroboration also failed"),
            "the peer-credential detail must be appended: {message}"
        );
    }
}

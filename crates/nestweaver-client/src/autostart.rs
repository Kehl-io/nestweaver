//! Ensures a daemon is running for a given database, auto-starting if needed.

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

/// Internal locked-FD handoff used only by a parent that owns [`SpawnLock`]
/// across spawning `nestweaver daemon start`.
pub const PARENT_SPAWN_LOCK_FD_ENV: &str = "NESTWEAVER_PARENT_SPAWN_LOCK_FD";

/// Serializes daemon spawns for one runtime instance.
///
/// Version-mismatch restarts acquire this before shutdown and retain it until
/// the replacement is ready, so a concurrent config-less caller cannot win
/// the gap between the old daemon releasing its pidfile and the replacement
/// publishing its socket.
pub struct SpawnLock {
    file: fs::File,
    instance_id: String,
    unlock_on_drop: AtomicBool,
}

impl SpawnLock {
    pub fn acquire(db_path: &Path) -> Result<Self> {
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
        let spawn_lock_path = pidfile.with_extension("spawnlock");
        Self::acquire_at(instance_id, &spawn_lock_path)
    }

    /// Acquire the blocking OS flock without blocking a Tokio executor thread.
    pub async fn acquire_async(db_path: &Path) -> Result<Self> {
        let db_path = db_path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::acquire(&db_path))
            .await
            .context("spawn-lock acquisition task failed")?
    }

    /// Configure a child command to inherit a duplicate of this exact locked
    /// file description across `exec`.
    pub fn configure_child_handoff(&self, command: &mut std::process::Command) -> Result<()> {
        let handoff = self
            .file
            .try_clone()
            .context("duplicate locked spawnlock for child handoff")?;
        let fd = handoff.as_raw_fd();
        command.env(PARENT_SPAWN_LOCK_FD_ENV, fd.to_string());
        // SAFETY: the pre-exec closure performs only async-signal-safe fcntl.
        // Capturing `handoff` keeps the descriptor alive in the parent Command
        // and in the forked child until exec; clearing CLOEXEC publishes it to
        // the new `daemon start` process.
        unsafe {
            command.pre_exec(move || {
                let _keep_handoff_alive = &handoff;
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // From this point onward the OS-managed lifetime of the shared open
        // file description is authoritative. Explicit LOCK_UN in either the
        // parent or child would unlock every duplicate prematurely.
        self.unlock_on_drop.store(false, Ordering::Release);
        Ok(())
    }

    /// Adopt the locked file description inherited from an owning parent.
    /// Ambient FD numbers are untrusted: duplicate first, validate the
    /// duplicate against the exact no-follow path, and close the original only
    /// after validation succeeds.
    pub fn inherit_parent_handoff(db_path: &Path, inherited_fd: RawFd) -> Result<Self> {
        anyhow::ensure!(
            inherited_fd >= 3,
            "invalid inherited parent spawnlock FD {inherited_fd}"
        );
        let duplicated_fd = unsafe { libc::fcntl(inherited_fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicated_fd == -1 {
            bail!(
                "cannot duplicate inherited parent spawnlock FD {inherited_fd}: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned here.
        let inherited = unsafe { fs::File::from_raw_fd(duplicated_fd) };
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
        let spawn_lock_path = pidfile.with_extension("spawnlock");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        let expected = options.open(&spawn_lock_path).with_context(|| {
            format!(
                "inherited parent spawnlock cannot be matched because {} cannot be opened",
                spawn_lock_path.display()
            )
        })?;
        let inherited_meta = inherited
            .metadata()
            .context("stat inherited parent spawnlock")?;
        let expected_meta = expected
            .metadata()
            .with_context(|| format!("stat expected spawnlock {}", spawn_lock_path.display()))?;
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            inherited_meta.file_type().is_file(),
            "inherited parent spawnlock FD is not a regular file"
        );
        anyhow::ensure!(
            inherited_meta.dev() == expected_meta.dev()
                && inherited_meta.ino() == expected_meta.ino(),
            "inherited parent spawnlock FD does not identify {}",
            spawn_lock_path.display()
        );
        anyhow::ensure!(
            !try_acquire_pidfile_lock(&expected)?,
            "inherited parent spawnlock {} is not locked",
            spawn_lock_path.display()
        );
        let inherited_lock =
            unsafe { libc::flock(inherited.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        anyhow::ensure!(
            inherited_lock == 0,
            "inherited parent spawnlock FD identifies the right file but does not carry the parent's locked open-file description"
        );
        unsafe {
            libc::close(inherited_fd);
        }
        Ok(Self {
            file: inherited,
            instance_id,
            // This lock belongs to the shared open-file description handed
            // off by the parent. Closing our duplicate is sufficient; an
            // explicit LOCK_UN would also unlock the parent's descriptor.
            unlock_on_drop: AtomicBool::new(false),
        })
    }

    fn acquire_at(instance_id: String, spawn_lock_path: &Path) -> Result<Self> {
        if let Some(parent) = spawn_lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create spawn lock directory {}", parent.display())
            })?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(spawn_lock_path)
            .with_context(|| format!("failed to open spawn lock {}", spawn_lock_path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!(
                "flock on spawn lock failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self {
            file,
            instance_id,
            unlock_on_drop: AtomicBool::new(true),
        })
    }

    /// Close a fork-inherited descriptor without issuing `LOCK_UN`.
    ///
    /// `flock` state is shared by file-description copies across `fork`. The
    /// daemonized child must close its duplicate while the launcher keeps the
    /// same lock through readiness and provenance attestation; explicitly
    /// unlocking here would prematurely release the launcher's lock too.
    pub fn close_in_forked_child_without_unlock(self) {
        let this = std::mem::ManuallyDrop::new(self);
        unsafe {
            libc::close(this.file.as_raw_fd());
        }
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        if self.unlock_on_drop.load(Ordering::Acquire) {
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Exclusive proof that no daemon owns the current pidfile inode.
///
/// Used by an explicit cold `daemon restart` only after acquiring SpawnLock
/// and rechecking the socket. Stale sidecar contents are deliberately not read.
pub struct UnownedPidfileLock {
    file: fs::File,
    owns_lock: bool,
}

impl UnownedPidfileLock {
    pub fn acquire(db_path: &Path) -> Result<Self> {
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
        if let Some(parent) = pidfile.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime dir: {}", parent.display()))?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(&pidfile)
            .with_context(|| format!("open pidfile: {}", pidfile.display()))?;
        anyhow::ensure!(
            try_acquire_pidfile_lock(&file)?,
            "daemon pidfile {} is still owned; refusing a cold restart over a live or shutting-down daemon",
            pidfile.display()
        );
        Ok(Self {
            file,
            owns_lock: true,
        })
    }

    pub fn release(&mut self) {
        if self.owns_lock {
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
            self.owns_lock = false;
        }
    }
}

impl Drop for UnownedPidfileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Attempt to own this exact pidfile inode. `true` is authoritative evidence
/// that no daemon holds it, regardless of whether its numeric contents happen
/// to name a live (recycled) process.
fn try_acquire_pidfile_lock(file: &fs::File) -> std::io::Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

fn requested_config_remedy(db_path: &Path, requested: &crate::RestartConfig) -> String {
    format!(
        "Run `nestweaver daemon --db {} restart --config {}` to apply the requested configuration",
        db_path.display(),
        requested
            .as_path()
            .expect("explicit config is always configured")
            .display()
    )
}

/// Attest an explicit config using the exact pidfile inode whose held flock
/// the caller has already observed. This synchronous seam protects public
/// `ensure_daemon*` callers that do not have a gRPC HealthCheck; the async
/// `DaemonClient` path performs the stronger HealthCheck/instance attestation
/// again before returning.
fn attest_requested_config_on_held_pidfile(
    db_path: &Path,
    requested_path: &Path,
    pidfile: &mut fs::File,
) -> Result<()> {
    let requested = crate::RestartConfig::for_cold_start(Some(requested_path))?;
    let remedy = requested_config_remedy(db_path, &requested);
    let pid = read_pid_from_file(pidfile).ok_or_else(|| {
        anyhow::anyhow!(
            "running daemon pidfile has no valid PID; effective config is unknown. {remedy}"
        )
    })?;
    anyhow::ensure!(pid > 0, "running daemon pidfile PID is invalid; {remedy}");
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let binding = nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
        &instance_id,
        pid as u32,
    )
    .with_context(|| {
        format!(
            "cannot verify the running daemon's effective config; effective config is unknown. {remedy}"
        )
    })?;
    let effective = crate::select_restart_config(None, || Ok(binding)).with_context(|| {
        format!(
            "cannot verify the running daemon's effective config; effective config is unknown. {remedy}"
        )
    })?;
    if effective != requested {
        let effective = match effective {
            crate::RestartConfig::Configured(path) => path.display().to_string(),
            crate::RestartConfig::CompiledDefaults => "compiled defaults".to_string(),
        };
        anyhow::bail!(
            "explicit --config {} does not match the running daemon's effective config ({effective}). {remedy}",
            requested.as_path().unwrap().display()
        );
    }
    Ok(())
}

/// Ensure a daemon is running for the given DB and return the socket path.
///
/// Acquires an exclusive flock on the pidfile, checks whether a live daemon
/// already exists, and spawns one if not. Uses exponential-backoff polling
/// to wait for the socket to accept connections.
pub fn ensure_daemon(db_path: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
    ensure_daemon_impl(db_path, config_path, true)
}

fn ensure_daemon_impl(
    db_path: &Path,
    config_path: Option<&Path>,
    attest_existing: bool,
) -> Result<PathBuf> {
    // An explicit path is a security-relevant assertion, not a child-process
    // hint. Validate it before creating runtime directories, taking locks,
    // cleaning sockets, or spawning anything so malformed proxy/MCP config
    // errors surface immediately with the real IO/TOML cause.
    let explicit_config = match config_path {
        Some(path) => Some(crate::RestartConfig::for_cold_start(Some(path))?),
        None => None,
    };
    let config_path = explicit_config
        .as_ref()
        .and_then(crate::RestartConfig::as_path);
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let rt_dir = nestweaver_daemon::lifecycle::runtime_dir(&instance_id);
    let sock = nestweaver_daemon::lifecycle::socket_path(&instance_id);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

    fs::create_dir_all(&rt_dir)
        .with_context(|| format!("failed to create runtime dir {}", rt_dir.display()))?;

    // Open pidfile for read+write (create if missing).
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&pidfile)
        .with_context(|| format!("failed to open pidfile {}", pidfile.display()))?;

    // Try a non-blocking exclusive flock. Every daemon server owns this lock
    // for its process lifetime, regardless of whether launchd, a foreground
    // child, or a non-macOS daemonized launcher created it.
    let fd = file.as_raw_fd();
    match try_acquire_pidfile_lock(&file) {
        Ok(false) => {
            // Lock held by the running daemon — it's alive.
            if let Some(pid) = read_pid_from_file(&mut file) {
                debug!(pid, "daemon already running (lock held)");
            }
            // The socket inode can appear before the listener is ready.
            wait_for_socket(&sock)?;
            if attest_existing && let Some(config_path) = config_path {
                attest_requested_config_on_held_pidfile(db_path, config_path, &mut file)?;
            }
            return Ok(sock);
        }
        Ok(true) => {}
        Err(error) => bail!("flock on pidfile failed: {error}"),
    }

    // We acquired the lock, so no daemon owns this pidfile. Its numeric PID is
    // stale even if the kernel has recycled that number for another live
    // process; never mistake numeric liveness for daemon ownership.
    if let Some(pid) = read_pid_from_file(&mut file) {
        warn!(pid, "unowned daemon pidfile is stale — cleaning up");
    }

    // Clean up a stale socket — but never one a live daemon is still serving.
    // See [`remove_socket_unless_served`]; the spawn-lock re-check below adopts
    // a surviving incumbent.
    remove_socket_unless_served(&sock);

    // Before spawning, check for a legacy daemon using the old DefaultHasher-
    // based instance ID (pre-SHA-256 upgrade). If found, shut it down so the
    // new daemon can acquire the DB write lock.
    nestweaver_daemon::lifecycle::stop_legacy_hash_daemon(db_path);

    // Same hazard, different superseded identity: nw-145 anchored instance ids
    // to the stable base path, so a daemon started by 6.3.0 while a CURRENT
    // pointer existed is bound to the SELECTED-SLOT id. It still holds the DB
    // write lock and is unreachable at the new socket path, so retire it here
    // too — otherwise the upgrade reintroduces exactly the orphaning nw-145
    // exists to prevent.
    nestweaver_daemon::lifecycle::stop_selected_slot_identity_daemon(db_path);

    // Also check for a legacy daemon that may hold the DB write lock at an
    // old $TMPDIR-based socket path (pre-v0.26.2 used $TMPDIR which varies
    // across launchers on macOS). If found, shut it down so the new daemon
    // can acquire the lock.
    stop_legacy_daemon(&instance_id);

    // Release the pidfile flock before spawning so the daemon can acquire it for its lifetime.
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);

    // Serialize concurrent auto-starts on a SEPARATE spawn-lock. The pidfile
    // flock had to be released above for the daemon to take it.
    let spawn_lock = SpawnLock::acquire(db_path)?;
    ensure_daemon_with_spawn_lock_impl(
        db_path,
        config_path,
        &spawn_lock,
        ColdStartSelection::Automatic,
    )
}

/// Async-safe wrapper around the synchronous pidfile/spawn/socket-readiness
/// protocol. All filesystem flocks and polling sleeps run off the executor.
pub async fn ensure_daemon_async(db_path: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
    let db_path = db_path.to_path_buf();
    let config_path = config_path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || ensure_daemon(&db_path, config_path.as_deref()))
        .await
        .context("daemon ensure task failed")?
}

/// Preserve an explicit config as a possible spawn argument, but defer
/// incumbent attestation until DaemonClient has checked its protocol version.
/// This keeps the verified old-version upgrade path available.
pub(crate) async fn ensure_daemon_for_client_async(
    db_path: &Path,
    config_path: Option<&Path>,
) -> Result<PathBuf> {
    let db_path = db_path.to_path_buf();
    let config_path = config_path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || ensure_daemon_impl(&db_path, config_path.as_deref(), false))
        .await
        .context("daemon ensure task failed")?
}

/// Start a daemon while the caller retains the instance's spawn lock.
///
/// This is the commit half of a version-mismatch restart. The caller acquires
/// the guard before shutting down the old daemon and passes the same guard
/// through replacement readiness.
pub fn ensure_daemon_with_spawn_lock(
    db_path: &Path,
    config_path: Option<&Path>,
    spawn_lock: &SpawnLock,
) -> Result<PathBuf> {
    ensure_daemon_with_spawn_lock_impl(db_path, config_path, spawn_lock, ColdStartSelection::Exact)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdStartSelection {
    /// Caller is auto-starting after proving no incumbent; reuse durable intent.
    Automatic,
    /// Caller already captured a live restart decision; `None` means defaults.
    Exact,
}

fn ensure_daemon_with_spawn_lock_impl(
    db_path: &Path,
    config_path: Option<&Path>,
    spawn_lock: &SpawnLock,
    selection: ColdStartSelection,
) -> Result<PathBuf> {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    anyhow::ensure!(
        spawn_lock.instance_id == instance_id,
        "spawn lock belongs to daemon instance {}, not {instance_id}",
        spawn_lock.instance_id
    );
    let sock = nestweaver_daemon::lifecycle::socket_path(&instance_id);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
    let restart_config = match selection {
        ColdStartSelection::Automatic => {
            crate::RestartConfig::for_automatic_cold_start(db_path, config_path)?
        }
        ColdStartSelection::Exact => crate::RestartConfig::for_cold_start(config_path)?,
    };

    // Re-check: another client may have started the daemon while we waited for the spawn-lock.
    if socket_accepts_connections(&sock) {
        debug!("daemon started by a concurrent client while awaiting spawn lock");
        // The winner is not trusted merely because it accepted a Unix
        // connection. It must attest the same automatic/captured config plan
        // before this contender releases the transaction lock.
        wait_for_daemon_ready(db_path, None, &restart_config, None)?;
        return Ok(sock);
    }

    // Whatever PID the pidfile names right now belongs to a daemon that is
    // already gone (we only reach here when nothing holds the lock). Capture it
    // so the readiness wait does not mistake it for the daemon we are about to
    // spawn.
    let stale_pid = read_pid(&pidfile);

    // nw-359 leg (1). The LAST moment before a process is created, and the
    // first one at which nothing else holds this database — the warm paths
    // returned long ago and the concurrent-starter re-check just above passed,
    // so this costs nothing except on a genuine cold start.
    //
    // Autostart was, until here, a TRANSPORT decision made with no knowledge of
    // STORAGE state: not one frame in `ensure_daemon_impl` opened, stat'd or
    // classified the database. Meanwhile the product's own recovery runbook for
    // an unreadable log opens with "Stop everything that opens this database
    // (daemon, watcher, UI)", and starting a daemon against that state is what
    // produces the crash-restart loop the operator then sees as "daemon process
    // exited before becoming healthy" — a message about the spawn, describing a
    // problem in the database.
    //
    // The probe is deliberately narrow and fails OPEN: it refuses only for a
    // log no open can replay. An unreplayed WAL, nw-367's checkpoint debris, a
    // database that does not exist yet, and a probe that fails for any other
    // reason all still spawn. See `db_wal_unreadable` for why each exclusion is
    // load-bearing rather than an oversight.
    if let Some(error) = nestweaver_daemon::lifecycle::db_wal_unreadable(db_path) {
        return Err(anyhow::Error::new(error).context(format!(
            "refusing to start a daemon against the database at {}",
            db_path.display()
        )));
    }

    // Spawn the daemon as a detached child.
    let mut launcher = spawn_daemon(db_path, restart_config.as_path(), spawn_lock)?;

    // Poll until the socket accepts connections, then release the spawn-lock so the next
    // waiter's re-check observes a ready daemon instead of spawning another.
    //
    // Watch the pidfile here: this is the one path where the daemon we are
    // waiting on was just spawned by us, so a process that has already exited
    // means it will never bind and there is nothing to wait for.
    let waited = wait_for_daemon_ready(db_path, stale_pid, &restart_config, Some(&mut launcher));
    waited?;

    info!("daemon started, socket at {}", sock.display());
    Ok(sock)
}

/// Async-safe guarded spawn/readiness commit. The guard is moved into the
/// blocking task and returned only after readiness, letting the async caller
/// retain it through successor identity/config verification.
pub async fn ensure_daemon_with_spawn_lock_async(
    db_path: &Path,
    config_path: Option<&Path>,
    spawn_lock: SpawnLock,
) -> Result<(PathBuf, SpawnLock)> {
    let db_path = db_path.to_path_buf();
    let config_path = config_path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        let socket = ensure_daemon_with_spawn_lock(&db_path, config_path.as_deref(), &spawn_lock)?;
        Ok((socket, spawn_lock))
    })
    .await
    .context("guarded daemon spawn/readiness task failed")?
}

/// Read a PID from an already-opened pidfile.
fn read_pid_from_file(file: &mut fs::File) -> Option<i32> {
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse::<i32>().ok()
}

/// Read PID from a pidfile path.
pub fn read_pid(path: &Path) -> Option<i32> {
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<i32>().ok()
}

/// Check whether a process is alive using `kill(pid, 0)`.
pub fn is_process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Override for how long an autostarted EPHEMERAL (temp-path) daemon idles
/// before self-terminating, distinct from `daemon start --idle-timeout`'s own
/// 3600s default.
pub const EPHEMERAL_IDLE_TIMEOUT_ENV: &str = "NESTWEAVER_EPHEMERAL_IDLE_TIMEOUT_SECS";

/// Review follow-up on nw-088 leg (2) (FIX 4). `is_temp_db_path` is a path
/// HEURISTIC — it was already trusted for macOS launchd-vs-foreground
/// routing and gc reclaimability, where a wrong answer costs nothing worse
/// than cosmetics. Reusing it here changes what a false positive COSTS: a
/// real, long-lived database that happens to live under `$TMPDIR`,
/// `/var/folders`, or a container whose working root IS `/tmp` now gets its
/// daemon's idle budget silently cut to ~60s, with no way for the operator
/// to say "this one is real" — the false-positive case is worse than the bug
/// leg (2) exists to fix. Setting this to any non-empty value makes
/// `daemon_start_command` treat `db_path` as real regardless of what
/// `is_temp_db_path` would answer, so the explicit signal always wins over
/// the heuristic.
pub const REAL_DB_UNDER_TEMP_ROOT_ENV: &str = "NESTWEAVER_REAL_DB_UNDER_TEMP_ROOT";

/// nw-088 leg (2). `daemon start`'s `--idle-timeout` default (3600s) is sized
/// for an interactive session against a REAL database. Autostart never
/// overrode it, so a one-shot command against a scratch `--db` under `/tmp` —
/// every test, every throwaway repro — left an hour-long resident daemon
/// behind it: proven live in the 2026-09-04 QA pass, which watched a plain
/// `index --repo <scratch> --db /tmp/.../s.lbug` do exactly that.
const DEFAULT_EPHEMERAL_IDLE_TIMEOUT_SECS: u64 = 60;

/// Resolve the ephemeral idle timeout, clamped to 1s or more. An unparseable
/// or zero override falls back to the default rather than failing the
/// command — this is a patience knob, not a correctness input, mirroring
/// [`daemon_boot_timeout`]'s own fallback discipline.
fn ephemeral_idle_timeout_secs() -> u64 {
    std::env::var(EPHEMERAL_IDLE_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_EPHEMERAL_IDLE_TIMEOUT_SECS)
}

/// Is the caller EXPLICITLY telling us `db_path`, however it is classified by
/// [`nestweaver_daemon::lifecycle::is_temp_db_path`], is a real database that
/// must keep the normal idle-timeout budget? See
/// [`REAL_DB_UNDER_TEMP_ROOT_ENV`] for why this override must exist at all.
fn caller_asserts_real_db_under_temp_root() -> bool {
    std::env::var_os(REAL_DB_UNDER_TEMP_ROOT_ENV).is_some_and(|value| !value.is_empty())
}

fn daemon_start_command(
    exe: &Path,
    db_path: &Path,
    config_path: Option<&Path>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "--db"]).arg(db_path).arg("start");
    // nw-088 leg (2). One predicate governs "is this path ephemeral" across
    // the whole daemon lifecycle — macOS launchd-vs-foreground-child routing
    // and the gc sweep's reclaimability already use
    // `nestweaver_daemon::lifecycle::is_temp_db_path`; using it here too is
    // the same guard, not a second guess that can drift from the other two.
    //
    // The explicit escape (FIX 4) is checked FIRST and short-circuits the
    // heuristic entirely — an operator's assertion about their own database
    // always outranks a path-shape guess.
    if !caller_asserts_real_db_under_temp_root()
        && nestweaver_daemon::lifecycle::is_temp_db_path(db_path)
    {
        cmd.arg("--idle-timeout")
            .arg(ephemeral_idle_timeout_secs().to_string());
    }
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    cmd
}

/// How much of the launcher's stderr to keep for the failure report.
const LAUNCHER_STDERR_CAP: usize = 8 * 1024;

/// The `nestweaver daemon … start` process we spawned, kept alive so its exit
/// can be observed.
///
/// nw-309: this used to be `cmd.spawn()?;` — the `Child` dropped on the spot
/// and all three streams sent to `/dev/null`. Two things went with it. The exit
/// STATUS, which is the authoritative answer to "will this daemon ever bind"
/// (the launcher itself waits for health and reports), and the stderr that says
/// WHY. Without either, a daemon that died in 60 ms was indistinguishable from
/// one still booting, and the caller waited out the full 30 s ceiling.
struct SpawnedLauncher {
    child: std::process::Child,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl SpawnedLauncher {
    /// `Some(reason)` once the launcher has exited unsuccessfully.
    ///
    /// Non-blocking, and deliberately silent about a launcher that exited
    /// SUCCESSFULLY: that means the daemon booted, and the readiness wait is
    /// then waiting on something that genuinely is coming.
    fn failure(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        if status.success() {
            return None;
        }
        let stderr = self
            .stderr
            .lock()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if stderr.is_empty() {
            Some(format!("`daemon start` exited with {status}"))
        } else {
            Some(format!("`daemon start` exited with {status}: {stderr}"))
        }
    }
}

/// Spawn `nestweaver daemon --db <path> start [--config <path>]` as a detached child.
fn spawn_daemon(
    db_path: &Path,
    config_path: Option<&Path>,
    spawn_lock: &SpawnLock,
) -> Result<SpawnedLauncher> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;

    debug!(exe = %exe.display(), db = %db_path.display(), "spawning daemon");

    let mut cmd = daemon_start_command(&exe, db_path, config_path);
    spawn_lock.configure_child_handoff(&mut cmd)?;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn daemon via {}", exe.display()))?;

    // Drain stderr on a detached thread rather than reading it at failure time.
    // Reading on demand would block forever if the daemon grandchild inherited
    // the write end and kept it open, and an undrained pipe stalls a chatty
    // launcher once the OS buffer fills — either would turn a diagnostic into a
    // hang, which is worse than the stall being fixed.
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let sink = std::sync::Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Ok(mut held) = sink.lock() else { break };
                        if held.len() < LAUNCHER_STDERR_CAP {
                            held.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
            }
        });
    }

    Ok(SpawnedLauncher { child, stderr })
}

/// Check legacy $TMPDIR-based socket paths for an old daemon and stop it.
///
/// Pre-v0.26.2 derived the socket path from `$TMPDIR`, which varies across
/// macOS launchers. On upgrade, the old daemon may still be running at a
/// `$TMPDIR`-based path and holding the DB write lock. This function probes
/// common legacy locations, and if it finds a live daemon, sends SIGTERM to
/// allow the new daemon to start cleanly.
fn stop_legacy_daemon(instance_id: &str) {
    let uid = unsafe { libc::getuid() };
    // Probe the two bases that the old $TMPDIR-derived logic would have used:
    // the caller's current $TMPDIR and /tmp (the fallback when $TMPDIR is unset).
    // These are the exact paths that diverge across macOS launchers.
    let mut legacy_bases: Vec<PathBuf> = vec![PathBuf::from("/tmp")];
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let p = PathBuf::from(&tmpdir);
        if p.as_path() != Path::new("/tmp") {
            legacy_bases.push(p);
        }
    }

    for base in &legacy_bases {
        let legacy_dir = base.join(format!("nw-{uid}")).join(instance_id);
        let legacy_pid = legacy_dir.join("daemon.pid");
        let legacy_sock = legacy_dir.join("daemon.sock");

        if !legacy_pid.exists() && !legacy_sock.exists() {
            continue;
        }

        if let Some(pid) = read_pid(&legacy_pid)
            && is_process_alive(pid)
        {
            info!(
                pid,
                path = %legacy_dir.display(),
                "stopping legacy daemon at old TMPDIR-based path"
            );
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(2) && is_process_alive(pid) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if is_process_alive(pid) {
                warn!(
                    pid,
                    "legacy daemon did not exit after SIGTERM, sending SIGKILL"
                );
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        // Clean up stale files.
        let _ = fs::remove_file(&legacy_sock);
        let _ = fs::remove_file(&legacy_pid);
        let _ = fs::remove_dir(&legacy_dir);
    }
}

fn socket_accepts_connections(sock: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(sock).is_ok()
}

/// Retire a leftover socket, unless a process is still serving it. Returns
/// whether the socket was removed.
///
/// The caller reaches here having acquired the pidfile `flock`, which it treats
/// as proof that no daemon owns this instance. It is not. The lock lives on an
/// INODE, not on a path: once `daemon.pid` has been unlinked under a running
/// daemon — the single most likely thing an operator does while recovering a
/// stuck instance — the owner keeps its lock on a now-unlinked inode, the
/// caller's `create(true)` open makes a brand-new file, and the flock succeeds
/// against no contention at all. Unlinking the socket then strands a HEALTHY
/// daemon that no client can ever reach again, while every subsequent command
/// silently answers from the direct path.
///
/// A socket that accepts a connection is kernel-reported evidence that a
/// process is listening on it right now, and no amount of runtime-file
/// tampering can forge it. It outranks the flock, so it wins.
///
/// Racing a daemon that is exiting between the probe and the unlink only leaves
/// a socket file behind, which the next start removes: strictly the safe
/// direction.
fn remove_socket_unless_served(sock: &Path) -> bool {
    if !sock.exists() || socket_accepts_connections(sock) {
        return false;
    }
    fs::remove_file(sock).is_ok()
}

/// Override for how long a client waits for a daemon to bind its socket.
pub const DAEMON_BOOT_TIMEOUT_ENV: &str = "NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS";

/// Default ceiling for daemon boot.
///
/// This was 5s, which is too tight for a legitimate cold start: a
/// Metal-enabled daemon compiles shaders and loads the embed model before it
/// binds, and a large database opens sidecars first. The result was a FALSE
/// failure — the daemon was booting normally and the client gave up on it. That
/// flake cost three releases a manual CI re-run (nw-114).
///
/// A longer ceiling alone would trade one problem for another: a genuinely dead
/// daemon would take this long to report. [`wait_for_socket_watching`] resolves
/// that by watching process liveness, so the ceiling only applies while the
/// daemon is actually alive and working.
const DEFAULT_DAEMON_BOOT_TIMEOUT_SECS: u64 = 30;

/// Resolve the boot ceiling, clamped to 1..=600s. An unparseable or
/// out-of-range value falls back to the default rather than failing the
/// command — this is a patience knob, not a correctness input.
pub fn daemon_boot_timeout() -> Duration {
    std::env::var(DAEMON_BOOT_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| (1..=600).contains(secs))
        .map_or_else(
            || Duration::from_secs(DEFAULT_DAEMON_BOOT_TIMEOUT_SECS),
            Duration::from_secs,
        )
}

fn wait_for_daemon_ready(
    db_path: &Path,
    ignore_pid: Option<i32>,
    expected_config: &crate::RestartConfig,
    launcher: Option<&mut SpawnedLauncher>,
) -> Result<()> {
    let timeout = daemon_boot_timeout();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create runtime for daemon readiness")?;
    // Only the caller that SPAWNED the daemon can answer "is it already dead";
    // every other waiter passes `None` and keeps today's behaviour (nw-309).
    let mut probe = launcher.map(|l| move || l.failure());
    let abort: Option<crate::ReadinessAbort<'_>> = match probe.as_mut() {
        Some(f) => Some(f),
        None => None,
    };
    runtime.block_on(crate::DaemonClient::wait_ready(
        db_path,
        timeout,
        ignore_pid,
        expected_config,
        abort,
    ))?;
    Ok(())
}

/// Poll for the socket to accept connections with exponential backoff.
///
/// Initial delay: 50ms, max delay: 500ms. See
/// [`DEFAULT_DAEMON_BOOT_TIMEOUT_SECS`] for the ceiling.
fn wait_for_socket(sock: &Path) -> Result<()> {
    wait_for_socket_watching(sock, None, None)
}

/// As [`wait_for_socket`], but bails as soon as the daemon we are waiting on is
/// known to have exited.
///
/// Waiting out the full ceiling only makes sense while the daemon is alive and
/// still working. If the pidfile names a process that has exited, it will never
/// bind and further waiting only delays the report.
///
/// `ignore_pid` is the PID the pidfile held BEFORE we spawned, and it must be
/// skipped. After a crash the stale pidfile still names the DEAD previous
/// daemon until the new one overwrites it, so treating any dead PID as failure
/// aborts the very restart we just requested — which is exactly what broke
/// `daemon_crash_recovery`. A pidfile that cannot be read yet is likewise not
/// treated as death: a freshly spawned daemon writes it asynchronously.
/// Where to send an operator whose daemon failed to bind `sock`.
///
/// The instance id is recovered from the socket's parent directory name, which
/// is the only identifier available at this point in the failure path.
fn log_hint_for_socket(sock: &Path) -> String {
    let instance_id = sock
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().replace("nestweaver-", ""))
        .unwrap_or_else(|| "default".to_string());
    nestweaver_daemon::lifecycle::log_hint(&instance_id)
}

fn wait_for_socket_watching(
    sock: &Path,
    pidfile: Option<&Path>,
    ignore_pid: Option<i32>,
) -> Result<()> {
    let start = Instant::now();
    let timeout = daemon_boot_timeout();
    let mut delay = Duration::from_millis(50);
    let max_delay = Duration::from_millis(500);

    while start.elapsed() < timeout {
        if socket_accepts_connections(sock) {
            return Ok(());
        }
        if let Some(path) = pidfile
            && let Some(pid) = read_pid(path)
            && Some(pid) != ignore_pid
            && !is_process_alive(pid)
        {
            // Re-check the socket once: the daemon may have bound and exited
            // between our last poll and this liveness check.
            if socket_accepts_connections(sock) {
                return Ok(());
            }
            bail!(
                "daemon process {pid} exited before binding {}.\n\
                 Check the daemon logs for errors: {}",
                sock.display(),
                log_hint_for_socket(sock)
            );
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(max_delay);
    }

    // One final check after the loop.
    if socket_accepts_connections(sock) {
        return Ok(());
    }

    // Do NOT offer `--no-daemon` here. It is a CI-only escape hatch that
    // bypasses the single-writer lock, and `resolve_use_daemon` refuses it
    // outside CI anyway — so the suggestion was both unusable and, if forced,
    // exactly the WAL-corruption risk the daemon exists to prevent (nw-125).
    // A slow boot is the common cause, so name the knob that actually helps.
    bail!(
        "daemon socket at {} did not accept connections within {:.1}s.\n\
         Check the daemon logs for errors: {}\n\
         If it is simply slow to boot, raise {}; if another process holds the \
         database lock, stop that process.",
        sock.display(),
        timeout.as_secs_f64(),
        log_hint_for_socket(sock),
        DAEMON_BOOT_TIMEOUT_ENV
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn write_valid_config(dir: &Path, name: &str, instance: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                r#"instance_id = "{instance}"
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
                dir.join("snapshots").display(),
                dir.join("workspace").display()
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn explicit_config_is_validated_before_runtime_or_lock_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let malformed = dir.path().join("malformed.toml");
        fs::write(&malformed, "instance_id = [broken").unwrap();
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db);
        let runtime = nestweaver_daemon::lifecycle::runtime_dir(&instance_id);
        let _ = fs::remove_dir_all(&runtime);

        let error = ensure_daemon(&db, Some(&malformed)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("invalid --config"), "{message}");
        assert!(message.contains("malformed.toml"), "{message}");
        assert!(
            !runtime.exists(),
            "bad explicit config must fail before runtime-dir/pidfile mutation"
        );
    }

    /// F1: the pidfile flock is path-defeatable, so it must not authorise
    /// unlinking a socket that a live daemon is still serving.
    #[test]
    fn a_served_socket_survives_socket_cleanup() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing there at all: nothing to remove, and no error.
        let absent = dir.path().join("absent.sock");
        assert!(!remove_socket_unless_served(&absent));
        assert!(!absent.exists());

        // A leftover socket nobody is serving: retired, as before.
        let stale = dir.path().join("stale.sock");
        {
            let listener = UnixListener::bind(&stale).unwrap();
            drop(listener);
        }
        // Dropping the listener leaves the inode but closes the listen queue.
        assert!(stale.exists());
        assert!(!socket_accepts_connections(&stale));
        assert!(remove_socket_unless_served(&stale));
        assert!(!stale.exists());

        // A socket a daemon is serving RIGHT NOW: never removed, whatever the
        // pidfile says or does not say. This is the incident: an operator ran
        // `rm daemon.pid`, so the client's flock succeeded trivially against a
        // brand-new inode while the daemon kept serving this socket.
        let live = dir.path().join("live.sock");
        let listener = UnixListener::bind(&live).unwrap();
        assert!(socket_accepts_connections(&live));
        assert!(
            !remove_socket_unless_served(&live),
            "a served socket must never be unlinked — doing so strands a healthy daemon"
        );
        assert!(
            live.exists(),
            "the live daemon's socket must still be there"
        );
        drop(listener);
        let _ = fs::remove_file(&live);
    }

    #[test]
    fn acquired_pidfile_flock_makes_even_a_live_numeric_pid_stale() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&pidfile)
            .unwrap();
        write!(file, "{}", std::process::id()).unwrap();
        assert!(is_process_alive(std::process::id() as i32));
        assert!(
            try_acquire_pidfile_lock(&file).unwrap(),
            "acquiring the flock is authoritative: no daemon owns this pidfile"
        );
        assert_eq!(
            read_pid_from_file(&mut file),
            Some(std::process::id() as i32)
        );
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[test]
    fn ensure_daemon_held_pidfile_rejects_a_different_config() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let configured = write_valid_config(dir.path(), "configured.toml", "configured");
        let requested = write_valid_config(dir.path(), "requested.toml", "requested");
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db);
        let runtime = nestweaver_daemon::lifecycle::runtime_dir(&instance_id);
        let pidfile_path = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
        let socket = nestweaver_daemon::lifecycle::socket_path(&instance_id);
        fs::create_dir_all(&runtime).unwrap();
        let mut owner = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&pidfile_path)
            .unwrap();
        writeln!(owner, "{}", std::process::id()).unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        nestweaver_daemon::lifecycle::write_effective_config_binding(
            &instance_id,
            &nestweaver_daemon::lifecycle::EffectiveConfigBinding::new(
                std::process::id(),
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: fs::canonicalize(&configured)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                },
            ),
        )
        .unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            drop(listener.accept().unwrap());
        });

        let error = ensure_daemon(&db, Some(&requested)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(configured.file_name().unwrap().to_str().unwrap()));
        assert!(message.contains(requested.file_name().unwrap().to_str().unwrap()));
        assert!(message.contains("restart --config"));

        server.join().unwrap();
        unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_UN) };
        drop(owner);
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn spawn_lock_blocks_a_concurrent_configless_contender_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.spawnlock");
        let first = SpawnLock::acquire_at("same-instance".to_string(), &lock_path).unwrap();
        let contender_path = lock_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            let second =
                SpawnLock::acquire_at("same-instance".to_string(), &contender_path).unwrap();
            tx.send(()).unwrap();
            drop(second);
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "contender must remain blocked across shutdown and replacement spawn"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("contender should proceed after transaction releases spawn lock");
        contender.join().unwrap();
    }

    #[test]
    fn inherited_fd_is_validated_and_keeps_lock_continuity() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let parent = SpawnLock::acquire(&db).unwrap();
        let inherited_fd = parent.file.try_clone().unwrap().into_raw_fd();
        let child = SpawnLock::inherit_parent_handoff(&db, inherited_fd).unwrap();

        // Parent death/close alone must not release the transaction; the
        // adopted duplicate shares the same locked open-file description.
        parent.close_in_forked_child_without_unlock();
        let contender_db = db.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            let lock = SpawnLock::acquire(&contender_db).unwrap();
            tx.send(()).unwrap();
            drop(lock);
        });
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        drop(child);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        contender.join().unwrap();
    }

    #[test]
    fn inherited_fd_rejects_missing_and_wrong_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        // Use a descriptor outside the process table rather than closing a
        // low-numbered FD that another parallel test could immediately reuse.
        let missing = RawFd::MAX;
        let error = match SpawnLock::inherit_parent_handoff(&db, missing) {
            Err(error) => error,
            Ok(_) => panic!("closed inherited FD must be rejected"),
        };
        assert!(format!("{error:#}").contains("cannot duplicate"));

        // Ensure the expected path exists, then supply a different regular
        // file descriptor. Validation must reject it without closing the
        // untrusted original descriptor.
        drop(SpawnLock::acquire(&db).unwrap());
        let wrong = fs::File::open("/dev/null").unwrap().into_raw_fd();
        let error = match SpawnLock::inherit_parent_handoff(&db, wrong) {
            Err(error) => error,
            Ok(_) => panic!("wrong inherited file must be rejected"),
        };
        assert!(format!("{error:#}").contains("not a regular file"));
        assert_ne!(unsafe { libc::fcntl(wrong, libc::F_GETFD) }, -1);
        unsafe { libc::close(wrong) };

        // The right inode is not sufficient: a separately opened descriptor
        // does not carry the parent's flock/open-file-description identity.
        let parent = SpawnLock::acquire(&db).unwrap();
        let spawn_lock_path = nestweaver_daemon::lifecycle::pidfile_path(
            &nestweaver_daemon::lifecycle::instance_id_from_db_path(&db),
        )
        .with_extension("spawnlock");
        let separate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(spawn_lock_path)
            .unwrap()
            .into_raw_fd();
        let error = match SpawnLock::inherit_parent_handoff(&db, separate) {
            Err(error) => error,
            Ok(_) => panic!("separate unlocked description must not authenticate"),
        };
        assert!(format!("{error:#}").contains("does not carry"));
        assert_ne!(unsafe { libc::fcntl(separate, libc::F_GETFD) }, -1);
        unsafe { libc::close(separate) };
        drop(parent);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_spawn_lock_contention_does_not_block_current_thread_timer() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let first = SpawnLock::acquire(&db).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            drop(first);
        });

        let second = tokio::time::timeout(Duration::from_secs(2), SpawnLock::acquire_async(&db))
            .await
            .expect("timer-driven holder release must run on a current-thread runtime")
            .unwrap();
        release.await.unwrap();
        drop(second);
    }

    #[test]
    fn daemon_start_command_does_not_pin_fork_routing() {
        let command = daemon_start_command(
            Path::new("/opt/nestweaver"),
            Path::new("/tmp/brain.lbug"),
            None,
        );

        assert!(
            command
                .get_envs()
                .all(|(name, _)| name != "NESTWEAVER_DAEMON_FORK"),
            "autostart must leave platform routing to `daemon start`"
        );
    }

    #[test]
    fn daemon_start_command_forwards_db_and_config() {
        // `tempfile::tempdir()` lives under the OS temp root by construction,
        // so this exercises the SAME `is_temp_db_path(db)` branch nw-088 leg
        // (2) added — the assertion below expects `--idle-timeout` for
        // exactly that reason, not despite it.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let spawn_lock = SpawnLock::acquire(&db).unwrap();
        let mut command = daemon_start_command(
            Path::new("/opt/nestweaver"),
            &db,
            Some(Path::new("/tmp/nestweaver-instance.toml")),
        );
        spawn_lock.configure_child_handoff(&mut command).unwrap();

        let args = command
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "daemon",
                "--db",
                db.to_str().unwrap(),
                "start",
                "--idle-timeout",
                &DEFAULT_EPHEMERAL_IDLE_TIMEOUT_SECS.to_string(),
                "--config",
                "/tmp/nestweaver-instance.toml",
            ]
            .map(std::ffi::OsString::from)
        );
        let fd = command
            .get_envs()
            .find_map(|(name, value)| (name == PARENT_SPAWN_LOCK_FD_ENV).then_some(value.unwrap()))
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<RawFd>()
            .unwrap();
        assert!(fd >= 3);
    }

    /// nw-088 leg (2)'s counterweight: a REAL (non-temp) database must keep
    /// today's behaviour exactly — no `--idle-timeout` override at all, so
    /// `daemon start`'s own 3600s default governs an interactive session.
    /// Without this, the ephemeral-path branch above could satisfy its own
    /// test by adding the flag unconditionally.
    #[test]
    fn daemon_start_command_does_not_override_idle_timeout_for_a_real_db() {
        let db = Path::new("/home/kory/projects/brain/brain.lbug");
        assert!(
            !nestweaver_daemon::lifecycle::is_temp_db_path(db),
            "precondition: this path must not be classified as ephemeral"
        );
        let command = daemon_start_command(Path::new("/opt/nestweaver"), db, None);
        let args = command
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            ["daemon", "--db", db.to_str().unwrap(), "start"].map(std::ffi::OsString::from)
        );
    }

    /// Review follow-up on nw-088 leg (2) (FIX 4). The path heuristic alone
    /// cannot tell a real database that happens to live under a temp root
    /// from a genuinely throwaway one — `NESTWEAVER_REAL_DB_UNDER_TEMP_ROOT`
    /// is the operator's explicit override, and it must win even though the
    /// path itself is unambiguously temp-shaped.
    #[test]
    fn daemon_start_command_honours_the_real_db_under_temp_root_escape() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        assert!(
            nestweaver_daemon::lifecycle::is_temp_db_path(&db),
            "precondition: this path must be classified as ephemeral"
        );

        // SAFETY: single-threaded test; the override is read only here.
        unsafe { std::env::set_var(REAL_DB_UNDER_TEMP_ROOT_ENV, "1") };
        let command = daemon_start_command(Path::new("/opt/nestweaver"), &db, None);
        unsafe { std::env::remove_var(REAL_DB_UNDER_TEMP_ROOT_ENV) };

        let args = command
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            ["daemon", "--db", db.to_str().unwrap(), "start"].map(std::ffi::OsString::from),
            "an explicit override must suppress --idle-timeout even for a \
             path the heuristic alone would call ephemeral"
        );
    }

    /// The counterweight: with the override ABSENT, the same temp-shaped
    /// path must still get the shortened timeout — otherwise the escape-hatch
    /// test above could pass because `daemon_start_command` stopped adding
    /// `--idle-timeout` for temp paths altogether, override or not.
    #[test]
    fn daemon_start_command_still_shortens_the_timeout_without_the_escape() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");

        // SAFETY: single-threaded test.
        unsafe { std::env::remove_var(REAL_DB_UNDER_TEMP_ROOT_ENV) };
        let command = daemon_start_command(Path::new("/opt/nestweaver"), &db, None);

        let args = command
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "daemon",
                "--db",
                db.to_str().unwrap(),
                "start",
                "--idle-timeout",
                &DEFAULT_EPHEMERAL_IDLE_TIMEOUT_SECS.to_string(),
            ]
            .map(std::ffi::OsString::from)
        );
    }

    /// nw-114: a daemon that has already exited must be reported immediately,
    /// not waited out to the full boot ceiling. Without the liveness check,
    /// raising the ceiling from 5s to 30s would have made every genuine
    /// start-up failure six times slower to report.
    #[test]
    fn wait_bails_at_once_when_the_daemon_pid_is_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let pidfile = dir.path().join("daemon.pid");

        // PID 1 is always alive; we need one that is certainly not. A freshly
        // reaped high PID is unreliable, so use an out-of-range value: kill(0)
        // reports ESRCH for it, which is exactly "no such process".
        std::fs::write(&pidfile, "2147483647").unwrap();

        let start = Instant::now();
        let result = wait_for_socket_watching(&socket, Some(&pidfile), None);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a dead daemon must not be waited out");
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("exited before binding"),
            "the error must say the process died, not just time out: {message}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must fail fast, took {elapsed:?}"
        );
    }

    /// A STALE pid from a crashed previous daemon must not abort the restart.
    ///
    /// This is the regression `daemon_crash_recovery` caught: after a crash the
    /// pidfile still names the dead previous daemon until the new one overwrites
    /// it, so treating any dead PID as failure kills the very restart we just
    /// requested.
    #[test]
    fn wait_ignores_the_stale_pid_it_was_told_to_skip() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let pidfile = dir.path().join("daemon.pid");

        // The pidfile still names a dead daemon from before the restart.
        let stale = 2147483647;
        std::fs::write(&pidfile, stale.to_string()).unwrap();

        // A "new daemon" binds shortly after, as a real restart would.
        let server_socket = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let listener = UnixListener::bind(&server_socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if listener.accept().is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let result = wait_for_socket_watching(&socket, Some(&pidfile), Some(stale));
        server.join().unwrap();

        assert!(
            result.is_ok(),
            "a stale pid must not abort the restart: {:#}",
            result.unwrap_err()
        );
    }

    /// The ceiling is env-overridable so CI and constrained machines can extend
    /// it without a rebuild. Out-of-range and unparseable values fall back to
    /// the default — this is a patience knob, not a correctness input.
    #[test]
    fn boot_timeout_honours_the_env_override_and_rejects_nonsense() {
        let default = Duration::from_secs(DEFAULT_DAEMON_BOOT_TIMEOUT_SECS);

        // SAFETY: single-threaded test; the override is read only here.
        unsafe { std::env::set_var(DAEMON_BOOT_TIMEOUT_ENV, "45") };
        assert_eq!(daemon_boot_timeout(), Duration::from_secs(45));

        for nonsense in ["0", "601", "abc", "", "-5"] {
            unsafe { std::env::set_var(DAEMON_BOOT_TIMEOUT_ENV, nonsense) };
            assert_eq!(
                daemon_boot_timeout(),
                default,
                "{nonsense:?} must fall back to the default"
            );
        }

        unsafe { std::env::remove_var(DAEMON_BOOT_TIMEOUT_ENV) };
        assert_eq!(daemon_boot_timeout(), default);
    }

    #[test]
    fn wait_for_socket_does_not_return_until_socket_accepts_connections() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");

        // Leave behind the exact state that caused the auto-start race: the
        // socket inode exists, but no process is listening yet.
        drop(UnixListener::bind(&socket).unwrap());

        let server_socket = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::remove_file(&server_socket).unwrap();
            let listener = UnixListener::bind(&server_socket).unwrap();
            listener.set_nonblocking(true).unwrap();

            let deadline = Instant::now() + Duration::from_secs(2);
            let mut accepted = 0;
            while accepted < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => accepted += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            accepted
        });

        let waited = wait_for_socket(&socket);
        let connect_after_wait = UnixStream::connect(&socket);
        let accepted = server.join().unwrap();

        assert!(waited.is_ok());
        assert!(
            connect_after_wait.is_ok(),
            "wait returned before the socket accepted connections"
        );
        assert_eq!(accepted, 2, "readiness probing should reach the listener");
    }
}

//! Ensures a daemon is running for a given database, auto-starting if needed.

use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

/// Serializes daemon spawns for one runtime instance.
///
/// Version-mismatch restarts acquire this before shutdown and retain it until
/// the replacement is ready, so a concurrent config-less caller cannot win
/// the gap between the old daemon releasing its pidfile and the replacement
/// publishing its socket.
pub struct SpawnLock {
    file: fs::File,
    instance_id: String,
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
            .open(&spawn_lock_path)
            .with_context(|| format!("failed to open spawn lock {}", spawn_lock_path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!(
                "flock on spawn lock failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { file, instance_id })
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
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

/// Ensure a daemon is running for the given DB and return the socket path.
///
/// Acquires an exclusive flock on the pidfile, checks whether a live daemon
/// already exists, and spawns one if not. Uses exponential-backoff polling
/// to wait for the socket to accept connections.
pub fn ensure_daemon(db_path: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
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

    // Clean up stale socket if present.
    if sock.exists() {
        let _ = fs::remove_file(&sock);
    }

    // Before spawning, check for a legacy daemon using the old DefaultHasher-
    // based instance ID (pre-SHA-256 upgrade). If found, shut it down so the
    // new daemon can acquire the DB write lock.
    nestweaver_daemon::lifecycle::stop_legacy_hash_daemon(db_path);

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
    ensure_daemon_with_spawn_lock(db_path, config_path, &spawn_lock)
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
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    anyhow::ensure!(
        spawn_lock.instance_id == instance_id,
        "spawn lock belongs to daemon instance {}, not {instance_id}",
        spawn_lock.instance_id
    );
    let sock = nestweaver_daemon::lifecycle::socket_path(&instance_id);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

    // Re-check: another client may have started the daemon while we waited for the spawn-lock.
    if socket_accepts_connections(&sock) {
        debug!("daemon started by a concurrent client while awaiting spawn lock");
        return Ok(sock);
    }

    // Whatever PID the pidfile names right now belongs to a daemon that is
    // already gone (we only reach here when nothing holds the lock). Capture it
    // so the readiness wait does not mistake it for the daemon we are about to
    // spawn.
    let stale_pid = read_pid(&pidfile);

    // Spawn the daemon as a detached child.
    spawn_daemon(db_path, config_path)?;

    // Poll until the socket accepts connections, then release the spawn-lock so the next
    // waiter's re-check observes a ready daemon instead of spawning another.
    //
    // Watch the pidfile here: this is the one path where the daemon we are
    // waiting on was just spawned by us, so a process that has already exited
    // means it will never bind and there is nothing to wait for.
    let waited = wait_for_socket_watching(&sock, Some(&pidfile), stale_pid);
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

fn daemon_start_command(
    exe: &Path,
    db_path: &Path,
    config_path: Option<&Path>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "--db"]).arg(db_path).arg("start");
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    cmd
}

/// Spawn `nestweaver daemon --db <path> start [--config <path>]` as a detached child.
fn spawn_daemon(db_path: &Path, config_path: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;

    debug!(exe = %exe.display(), db = %db_path.display(), "spawning daemon");

    let mut cmd = daemon_start_command(&exe, db_path, config_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn daemon via {}", exe.display()))?;

    Ok(())
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
fn daemon_boot_timeout() -> Duration {
    std::env::var(DAEMON_BOOT_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| (1..=600).contains(secs))
        .map_or_else(
            || Duration::from_secs(DEFAULT_DAEMON_BOOT_TIMEOUT_SECS),
            Duration::from_secs,
        )
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
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn acquired_pidfile_flock_makes_even_a_live_numeric_pid_stale() {
        use std::io::Write;

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

    #[tokio::test(flavor = "current_thread")]
    async fn async_ensure_production_seam_allows_timer_driven_transaction_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db);
        let socket = nestweaver_daemon::lifecycle::socket_path(&instance_id);
        let runtime = nestweaver_daemon::lifecycle::runtime_dir(&instance_id);
        let transaction_lock = SpawnLock::acquire(&db).unwrap();
        let holder_socket = socket.clone();
        let holder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(parent) = holder_socket.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let _ = fs::remove_file(&holder_socket);
            let listener = UnixListener::bind(&holder_socket).unwrap();
            drop(transaction_lock);
            listener
        });

        let ensured = tokio::time::timeout(Duration::from_secs(3), ensure_daemon_async(&db, None))
            .await
            .expect("blocking spawn-lock wait must not prevent the holder's timer")
            .unwrap();
        assert_eq!(ensured, socket);
        let listener = holder.await.unwrap();
        drop(listener);
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir_all(runtime);
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
        let command = daemon_start_command(
            Path::new("/opt/nestweaver"),
            Path::new("/tmp/brain.lbug"),
            Some(Path::new("/tmp/nestweaver-instance.toml")),
        );

        let args = command
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "daemon",
                "--db",
                "/tmp/brain.lbug",
                "start",
                "--config",
                "/tmp/nestweaver-instance.toml",
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

//! Ensures a daemon is running for a given database, auto-starting if needed.

use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

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

    // Try a non-blocking exclusive flock. The `daemonize2` crate holds
    // LOCK_EX on the pidfile for the daemon's entire lifetime, so if we
    // can't acquire it the daemon is definitely running.
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            // Lock held by the running daemon — it's alive.
            if let Some(pid) = read_pid_from_file(&mut file) {
                debug!(pid, "daemon already running (lock held)");
            }
            // The socket inode can appear before the listener is ready.
            wait_for_socket(&sock)?;
            return Ok(sock);
        }
        bail!("flock on pidfile failed: {}", err);
    }

    // We acquired the lock, so no daemon holds it. Check if a process
    // from the pidfile is still alive (shouldn't be, but be safe).
    if let Some(pid) = read_pid_from_file(&mut file) {
        if is_process_alive(pid) {
            debug!(pid, "daemon already running");
            unsafe { libc::flock(fd, libc::LOCK_UN) };
            wait_for_socket(&sock)?;
            return Ok(sock);
        }
        warn!(pid, "stale daemon pid — cleaning up");
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

    // Serialize concurrent auto-starts on a SEPARATE spawn-lock. The pidfile flock had to be
    // released above for the daemon to take it, which opens a window where a fleet of clients
    // starting at once could each spawn a daemon (only the DB write-lock would then absorb the
    // pile-up). Holding this blocking lock across spawn + socket-wait, with a re-check, means
    // exactly one client spawns and the rest observe the started daemon. Acquired AFTER releasing
    // the pidfile flock so the spawned daemon can take that flock (otherwise: deadlock).
    let spawn_lock_path = pidfile.with_extension("spawnlock");
    let spawn_lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&spawn_lock_path)
        .with_context(|| format!("failed to open spawn lock {}", spawn_lock_path.display()))?;
    if unsafe { libc::flock(spawn_lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!(
            "flock on spawn lock failed: {}",
            std::io::Error::last_os_error()
        );
    }

    // Re-check: another client may have started the daemon while we waited for the spawn-lock.
    if socket_accepts_connections(&sock) {
        unsafe { libc::flock(spawn_lock.as_raw_fd(), libc::LOCK_UN) };
        debug!("daemon started by a concurrent client while awaiting spawn lock");
        return Ok(sock);
    }

    // Spawn the daemon as a detached child.
    spawn_daemon(db_path, config_path)?;

    // Poll until the socket accepts connections, then release the spawn-lock so the next
    // waiter's re-check observes a ready daemon instead of spawning another.
    let waited = wait_for_socket(&sock);
    unsafe { libc::flock(spawn_lock.as_raw_fd(), libc::LOCK_UN) };
    waited?;

    info!("daemon started, socket at {}", sock.display());
    Ok(sock)
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

/// Spawn `nestweaver daemon --db <path> start [--config <path>]` as a detached child.
fn spawn_daemon(db_path: &Path, config_path: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;

    debug!(exe = %exe.display(), db = %db_path.display(), "spawning daemon");

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["daemon", "--db"]).arg(db_path).arg("start");
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    // Auto-spawned daemons are ephemeral — one per DB a client happens to touch —
    // so they must NOT install a persistent launchd agent. Doing so leaked
    // hundreds of `io.kehl.nestweaver.<hash>.plist` files with
    // KeepAlive{Crashed:true}, which then crash-looped. Force the fork-based
    // daemonization path here; launchd registration is reserved for an explicit
    // `nestweaver daemon start` invoked by the user (or the menubar app).
    cmd.env("NESTWEAVER_DAEMON_FORK", "1");
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

/// Poll for the socket to accept connections with exponential backoff.
///
/// Initial delay: 50ms, max delay: 500ms, total timeout: 5s.
/// Larger databases need more time to open the DB, load sidecars,
/// and bind the socket.
fn wait_for_socket(sock: &Path) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut delay = Duration::from_millis(50);
    let max_delay = Duration::from_millis(500);

    while start.elapsed() < timeout {
        if socket_accepts_connections(sock) {
            return Ok(());
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(max_delay);
    }

    // One final check after the loop.
    if socket_accepts_connections(sock) {
        return Ok(());
    }

    bail!(
        "daemon socket at {} did not accept connections within {:.1}s.\n\
         Check the daemon log for errors: {}\n\
         If another process holds the database lock, stop it or use --no-daemon.",
        sock.display(),
        timeout.as_secs_f64(),
        nestweaver_daemon::lifecycle::log_path(
            &sock
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().replace("nestweaver-", ""))
                .unwrap_or_else(|| "default".to_string())
        )
        .display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

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

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
/// to wait for the socket to appear.
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
            // Wait for the socket to appear if the daemon is still starting.
            if !sock.exists() {
                wait_for_socket(&sock)?;
            }
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

    // Release the flock before spawning so the daemon can acquire it.
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);

    // Spawn the daemon as a detached child.
    spawn_daemon(db_path, config_path)?;

    // Poll for socket to appear.
    wait_for_socket(&sock)?;

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

/// Poll for the socket file to appear with exponential backoff.
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
        if sock.exists() {
            return Ok(());
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(max_delay);
    }

    // One final check after the loop.
    if sock.exists() {
        return Ok(());
    }

    bail!(
        "daemon did not create socket at {} within {:.1}s.\n\
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

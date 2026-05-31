//! Ensures a daemon is running for a given database, auto-starting if needed.

use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// Ensure a daemon is running for the given DB and return the socket path.
///
/// Acquires an exclusive flock on the pidfile, checks whether a live daemon
/// already exists, and spawns one if not. Uses exponential-backoff polling
/// to wait for the socket to appear.
pub fn ensure_daemon(db_path: &Path) -> Result<PathBuf> {
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

    // Try a non-blocking exclusive flock. The `daemonize` crate holds
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

    // Release the flock before spawning so the daemon can acquire it.
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);

    // Spawn the daemon as a detached child.
    spawn_daemon(db_path)?;

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

/// Spawn `nestweaver daemon --db <path> start` as a detached child.
fn spawn_daemon(db_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;

    debug!(exe = %exe.display(), db = %db_path.display(), "spawning daemon");

    std::process::Command::new(&exe)
        .args(["daemon", "--db"])
        .arg(db_path)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn daemon via {}", exe.display()))?;

    Ok(())
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
        "daemon did not create socket at {} within {:.1}s",
        sock.display(),
        timeout.as_secs_f64()
    );
}

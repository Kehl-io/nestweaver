//! `ServerGuard` — RAII helper for server-mode integration tests.
//!
//! Spawns `nestweaver daemon --db <path> run --server --bind 127.0.0.1:0 --port-file <path>`
//! as a foreground child process, waits for the port file to appear, and kills
//! the process on drop.
//!
//! The child's stderr is piped and drained by a reader thread into a bounded
//! in-memory tail buffer. Draining is load-bearing: with the pipe undrained a
//! chatty server blocks on write once the OS pipe buffer (~64 KiB) fills, the
//! port file never appears, and the startup deadline fires with no useful
//! diagnostics. The captured tail is included in the timeout panic message.
//!
//! Startup deadline: 30 s by default (the old 10 s was too tight under
//! full-workspace parallel test load); override with
//! `NESTWEAVER_TEST_SERVER_TIMEOUT_SECS`.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much of the child's stderr to retain for failure diagnostics.
const STDERR_TAIL_CAP: usize = 16 * 1024;

/// Guard that owns a `nestweaver daemon run --server` child process.
///
/// On drop the child is killed and the port file removed, ensuring cleanup even
/// on test panics.
pub struct ServerGuard {
    child: Child,
    port_file: PathBuf,
    #[allow(dead_code)]
    db_path: PathBuf,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerGuard {
    /// Spawn the server without authentication.
    pub fn start(db_path: &Path) -> Self {
        Self::spawn(db_path, None)
    }

    /// Spawn the server with a bearer auth token.
    pub fn start_with_auth(db_path: &Path, token: &str) -> Self {
        Self::spawn_inner(db_path, Some(token), None, None, None, None, None, None)
    }

    /// Spawn the server with an instance config (`--config <instance.toml>`).
    ///
    /// Used by the federation tests: the config's `[[upstream]]` block makes the
    /// daemon build a federation coordinator at its `/mcp` boundary, so a raw
    /// MCP client gets two-tier results for two-tier-routed tools.
    pub fn start_with_config(db_path: &Path, config_path: &Path) -> Self {
        Self::spawn_inner(
            db_path,
            None,
            None,
            None,
            None,
            None,
            Some(config_path),
            None,
        )
    }

    /// Spawn the server with both a query auth token and an admin token.
    ///
    /// Used by the device-flow integration test: the admin token approves a
    /// pending grant, which then hands the developer the configured query token.
    pub fn start_with_admin_and_auth(db_path: &Path, auth_token: &str, admin_token: &str) -> Self {
        Self::spawn_inner(
            db_path,
            Some(auth_token),
            None,
            None,
            None,
            Some(admin_token),
            None,
            None,
        )
    }

    /// Spawn the server with query/admin tokens and an instance config.
    ///
    /// This lets process-level tests exercise authenticated MCP-HTTP requests
    /// under an enabled `[authz]` policy.
    pub fn start_with_admin_auth_and_config(
        db_path: &Path,
        auth_token: &str,
        admin_token: &str,
        config_path: &Path,
    ) -> Self {
        Self::spawn_inner(
            db_path,
            Some(auth_token),
            None,
            None,
            None,
            Some(admin_token),
            Some(config_path),
            None,
        )
    }

    /// Spawn the server with TLS enabled.
    pub fn start_with_tls(db_path: &Path, cert: &Path, key: &Path) -> Self {
        Self::spawn_inner(db_path, None, Some(cert), Some(key), None, None, None, None)
    }

    /// Spawn the server with a webhook secret configured.
    pub fn start_with_webhook(db_path: &Path, secret: &str) -> Self {
        Self::spawn_inner(db_path, None, None, None, Some(secret), None, None, None)
    }

    /// Spawn the server mid-rotation: both a current (`secret`) and previous
    /// (`secret_old`) webhook secret are accepted. Used to exercise the live
    /// dual-secret overlap window over real HTTP.
    pub fn start_with_webhook_rotation(db_path: &Path, secret: &str, secret_old: &str) -> Self {
        Self::spawn_inner(
            db_path,
            None,
            None,
            None,
            Some(secret),
            None,
            None,
            Some(secret_old),
        )
    }

    /// Return the TCP port the server bound to (read from the port file, line 1).
    pub fn grpc_port(&self) -> u16 {
        let contents =
            std::fs::read_to_string(&self.port_file).expect("port file should be readable");
        contents
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .parse::<u16>()
            .expect("port file line 1 should contain a valid u16 port number")
    }

    /// Return the MCP HTTP port (read from the port file, line 2).
    pub fn mcp_port(&self) -> u16 {
        let contents =
            std::fs::read_to_string(&self.port_file).expect("port file should be readable");
        contents
            .lines()
            .nth(1)
            .unwrap_or("")
            .trim()
            .parse::<u16>()
            .expect("port file line 2 should contain a valid u16 MCP port number")
    }

    /// Return the full gRPC address, e.g. `http://127.0.0.1:12345`.
    pub fn grpc_addr(&self) -> String {
        format!("http://127.0.0.1:{}", self.grpc_port())
    }

    /// Return the full MCP HTTP address, e.g. `http://127.0.0.1:12346`.
    pub fn mcp_addr(&self) -> String {
        format!("http://127.0.0.1:{}", self.mcp_port())
    }

    // ── internal ──────────────────────────────────────────────────────

    fn spawn(db_path: &Path, auth_token: Option<&str>) -> Self {
        Self::spawn_inner(db_path, auth_token, None, None, None, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        db_path: &Path,
        auth_token: Option<&str>,
        tls_cert: Option<&Path>,
        tls_key: Option<&Path>,
        webhook_secret: Option<&str>,
        admin_token: Option<&str>,
        config_path: Option<&Path>,
        webhook_secret_old: Option<&str>,
    ) -> Self {
        let port_file = db_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("server.port");

        // Remove any stale port file from a previous run.
        let _ = std::fs::remove_file(&port_file);

        let bin = env!("CARGO_BIN_EXE_nestweaver");

        let mut cmd = Command::new(bin);
        cmd.args([
            "daemon",
            "--db",
            &db_path.display().to_string(),
            "run",
            "--server",
            "--bind",
            "127.0.0.1:0",
            "--port-file",
            &port_file.display().to_string(),
        ]);

        if let Some(token) = auth_token {
            cmd.args(["--auth-token", token]);
        }

        if let Some(cert) = tls_cert {
            cmd.args(["--tls-cert", &cert.display().to_string()]);
        }
        if let Some(key) = tls_key {
            cmd.args(["--tls-key", &key.display().to_string()]);
        }

        if let Some(secret) = webhook_secret {
            cmd.args(["--webhook-secret", secret]);
        }

        if let Some(secret_old) = webhook_secret_old {
            cmd.args(["--webhook-secret-old", secret_old]);
        }

        if let Some(token) = admin_token {
            cmd.args(["--admin-token", token]);
        }

        if let Some(config) = config_path {
            cmd.args(["--config", &config.display().to_string()]);
        }

        let mut child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn nestweaver daemon in server mode");

        // Drain the child's stderr from a reader thread into a bounded tail
        // buffer. Without this the child blocks on write once the pipe buffer
        // fills and the startup deadline fires spuriously.
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_thread = child.stderr.take().map(|mut pipe| {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]);
                            let mut guard = tail.lock().unwrap();
                            guard.push_str(&chunk);
                            if guard.len() > STDERR_TAIL_CAP {
                                let mut start = guard.len() - STDERR_TAIL_CAP;
                                while !guard.is_char_boundary(start) {
                                    start += 1;
                                }
                                guard.drain(..start);
                            }
                        }
                    }
                }
            })
        });

        let mut guard = Self {
            child,
            port_file: port_file.clone(),
            db_path: db_path.to_path_buf(),
            stderr_thread,
        };

        // Wait for the port file to appear. The deadline scales: 30 s default
        // (parallel full-workspace test runs starve the child of CPU; the old
        // 10 s fired spuriously), overridable via env for even slower CI hosts.
        let timeout_secs: u64 = std::env::var("NESTWEAVER_TEST_SERVER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(30);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        while Instant::now() < deadline {
            if port_file.exists() {
                // Give the server a moment to finish binding after writing.
                std::thread::sleep(Duration::from_millis(100));
                return guard;
            }
            // Early-exit if the child died (e.g. bad flags) instead of
            // waiting out the full deadline.
            if let Ok(Some(status)) = guard.child.try_wait() {
                let tail = stderr_tail.lock().unwrap();
                panic!(
                    "nestweaver daemon in server mode exited early ({status}) \
                     before writing port file {port_file:?}\n\
                     --- captured stderr tail ---\n{tail}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let tail = stderr_tail.lock().unwrap();
        panic!(
            "port file {port_file:?} did not appear within {timeout_secs} s \
             (override with NESTWEAVER_TEST_SERVER_TIMEOUT_SECS)\n\
             --- captured stderr tail ---\n{tail}"
        );
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The stderr pipe hits EOF once the child is reaped; join the reader
        // thread so it doesn't linger across tests in the same process.
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.port_file);
    }
}

//! `ServerGuard` — RAII helper for server-mode integration tests.
//!
//! Spawns `nestweaver daemon --db <path> run --server --bind 127.0.0.1:0 --port-file <path>`
//! as a foreground child process, waits for the port file to appear, and kills
//! the process on drop.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Guard that owns a `nestweaver daemon run --server` child process.
///
/// On drop the child is killed and the port file removed, ensuring cleanup even
/// on test panics.
pub struct ServerGuard {
    child: Child,
    port_file: PathBuf,
    #[allow(dead_code)]
    db_path: PathBuf,
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
        Self::spawn_inner(db_path, None, None, None, None, None, Some(config_path), None)
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

        // Run in foreground — launchd-style daemonisation doesn't work in tests.
        cmd.env("NESTWEAVER_DAEMON_FORK", "0");

        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn nestweaver daemon in server mode");

        let guard = Self {
            child,
            port_file: port_file.clone(),
            db_path: db_path.to_path_buf(),
        };

        // Wait for the port file to appear (up to 10 s).
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if port_file.exists() {
                // Give the server a moment to finish binding after writing.
                std::thread::sleep(Duration::from_millis(100));
                return guard;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // If we reach here in the current codebase, it means `run_server()`
        // hasn't been implemented yet (Task 3). That's expected — tests that
        // call `start()` will be gated behind `#[ignore]` until then.
        //
        // We still panic so that un-ignored tests get a clear error.
        panic!(
            "port file {:?} did not appear within 10 s — \
             is `nestweaver daemon run --server` implemented?",
            port_file
        );
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.port_file);
    }
}

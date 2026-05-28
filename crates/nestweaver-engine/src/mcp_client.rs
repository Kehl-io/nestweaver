use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Default timeout for MCP tool calls (seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a single MCP `tools/call` invocation.
pub struct ToolCallResult {
    pub content: String,
    pub is_error: bool,
}

pub struct McpClient {
    stdin: std::process::ChildStdin,
    reader: std::io::BufReader<std::process::ChildStdout>,
    child: Child,
    next_id: u64,
    timeout: Duration,
    poisoned: bool,
}

impl McpClient {
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, anyhow::Error> {
        Self::spawn_with_timeout(command, args, env, DEFAULT_TIMEOUT)
    }

    pub fn spawn_with_timeout(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, anyhow::Error> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout unavailable"))?;
        let reader = BufReader::new(stdout);
        let mut client = Self {
            stdin,
            reader,
            child,
            next_id: 1,
            timeout,
            poisoned: false,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<(), anyhow::Error> {
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id(), "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "nestweaver", "version": "0.9.0" }
            }
        });
        self.send(&req)?;
        let _resp = self.recv()?;
        let notif = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.send(&notif)?;
        Ok(())
    }

    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolCallResult, anyhow::Error> {
        if self.poisoned {
            anyhow::bail!(
                "MCP client is in a failed state — previous call timed out or server crashed"
            );
        }
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id(), "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        self.send(&req)?;
        let resp = self.recv()?;
        let result_obj = resp.get("result");
        let content = result_obj
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let is_error = result_obj
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(ToolCallResult { content, is_error })
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: &serde_json::Value) -> Result<(), anyhow::Error> {
        if self.poisoned {
            anyhow::bail!("MCP client is poisoned");
        }
        serde_json::to_writer(&mut self.stdin, value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> Result<serde_json::Value, anyhow::Error> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() > deadline {
                self.poisoned = true;
                let _ = self.child.kill();
                anyhow::bail!(
                    "MCP server did not respond within {}s",
                    self.timeout.as_secs()
                );
            }

            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line)?;
            if bytes_read == 0 {
                self.poisoned = true;
                anyhow::bail!("MCP server closed stdout unexpectedly (EOF)");
            }
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line)?;
            if value.get("id").is_some() {
                return Ok(value);
            }
            tracing::trace!("MCP notification: {}", line.trim());
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

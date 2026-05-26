use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

pub struct McpClient {
    child: Child,
    next_id: u64,
}

impl McpClient {
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, anyhow::Error> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn()?;
        let mut client = Self { child, next_id: 1 };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<(), anyhow::Error> {
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id(), "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "nestweaver", "version": "0.1.0" }
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
    ) -> Result<String, anyhow::Error> {
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id(), "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        self.send(&req)?;
        let resp = self.recv()?;
        let content = resp
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        Ok(content)
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: &serde_json::Value) -> Result<(), anyhow::Error> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin unavailable"))?;
        serde_json::to_writer(&mut *stdin, value)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> Result<serde_json::Value, anyhow::Error> {
        let stdout = self
            .child
            .stdout
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdout unavailable"))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

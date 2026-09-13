//! A bounded server handshake, deliberately distinct from host activation.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: &str = nestweaver_mcp::protocol::PROTOCOL_VERSION;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

async fn response(reader: &mut BufReader<tokio::process::ChildStdout>, id: u64) -> Result<Value> {
    // Bound both bytes and messages. Notifications are legal while a request
    // is pending, but cannot extend the overall handshake deadline.
    for _ in 0..32 {
        let mut bytes = Vec::new();
        let read = reader
            .take(MAX_RESPONSE_BYTES + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        ensure!(read > 0, "MCP server closed stdout before response {id}");
        ensure!(
            read as u64 <= MAX_RESPONSE_BYTES,
            "MCP response exceeds 256 KiB"
        );
        let value: Value = serde_json::from_slice(&bytes).context("malformed MCP response")?;
        ensure!(value["jsonrpc"] == "2.0", "invalid MCP JSON-RPC version");
        if value.get("id").is_none() && value.get("method").is_some() {
            continue;
        }
        ensure!(value["id"] == id, "unexpected MCP response id");
        ensure!(
            value.get("error").is_none(),
            "MCP server rejected request {id}: {}",
            value["error"]
        );
        return value
            .get("result")
            .cloned()
            .context("MCP response has no result");
    }
    anyhow::bail!("too many MCP notifications before response {id}")
}

async fn probe(
    command: &str,
    args: &[String],
    base: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<usize> {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .current_dir(base)
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("launch configured MCP command")?;
    let result = tokio::time::timeout(timeout, async {
        let mut input = child.stdin.take().context("MCP stdin unavailable")?;
        let mut output = BufReader::new(child.stdout.take().context("MCP stdout unavailable")?);
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion": PROTOCOL, "capabilities":{},
            "clientInfo":{"name":"nestweaver-setup-probe","version":env!("CARGO_PKG_VERSION")}
        }});
        input.write_all(format!("{init}\n").as_bytes()).await?;
        let initialized = response(&mut output, 1).await?;
        ensure!(initialized["protocolVersion"] == PROTOCOL, "unsupported negotiated MCP protocol");
        ensure!(initialized["capabilities"]["tools"].is_object(), "server does not advertise MCP tools");
        input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n").await?;
        let listed = response(&mut output, 2).await?;
        let tools = listed["tools"].as_array().context("tools/list has no tools array")?;
        let lite = args.iter().any(|arg| arg == "--lite");
        let catalogue = nestweaver_mcp::tools::tool_list(lite);
        let mut expected: Vec<String> = catalogue["tools"].as_array().context("invalid built-in catalogue")?
            .iter().filter_map(|tool| tool["name"].as_str().map(String::from)).collect();
        if let Some(selection) = args.windows(2).find(|pair| pair[0] == "--tools") {
            let names: Vec<_> = selection[1].split(',').collect();
            ensure!(names.iter().all(|name| expected.iter().any(|candidate| candidate == name)), "configured MCP tool selection is unavailable in this profile");
            expected.retain(|name| names.contains(&name.as_str()));
        }
        for required in &expected {
            ensure!(tools.iter().any(|tool| tool["name"] == required.as_str() && tool["inputSchema"].is_object()), "configured MCP profile is missing {required}");
        }
        if lite || args.iter().any(|arg| arg == "--tools") {
            ensure!(tools.len() == expected.len(), "MCP server did not honor the selected tool profile");
        }
        ensure!(listed.get("nextCursor").is_none_or(Value::is_null), "unexpected paginated NestWeaver tool catalogue");
        Ok(tools.len())
    }).await.context("MCP server probe timed out").and_then(|result| result);
    // Always reap the exact child, including malformed/error/timeout paths.
    // kill_on_drop covers cancellation while the handshake future is pending.
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

/// Host-only interpolation and environment policies cannot be emulated by a
/// generic stdio probe. Refuse to execute a different context accidentally.
fn validate_probe_context(entry: &Value) -> Result<()> {
    ensure!(
        entry.get("disabled").and_then(Value::as_bool) != Some(true)
            && entry.get("enabled").and_then(Value::as_bool) != Some(false),
        "registration is disabled"
    );
    for key in [
        "cwd",
        "env_vars",
        "envFile",
        "experimental_environment",
        "url",
    ] {
        ensure!(
            entry.get(key).is_none(),
            "host-specific {key} requires activation verification in the host; generic probe skipped"
        );
    }
    fn interpolated(value: &Value) -> bool {
        match value {
            Value::String(text) => text.contains("${"),
            Value::Array(values) => values.iter().any(interpolated),
            Value::Object(values) => values.values().any(interpolated),
            _ => false,
        }
    }
    ensure!(
        !interpolated(entry),
        "host-specific variable interpolation requires verification in the host; generic probe skipped"
    );
    Ok(())
}

/// Repository registrations are untrusted input. A handshake does not make an
/// arbitrary executable safe: only this running NestWeaver binary and a bounded
/// MCP argument grammar may cross the subprocess boundary. Custom registrations
/// are preserved for the host, but never executed by setup.
fn validate_invocation(
    command: &str,
    args: &[String],
    base: &Path,
    env: &[(String, String)],
) -> Result<()> {
    ensure!(
        env.is_empty(),
        "custom MCP environment requires verification in the host; generic probe skipped"
    );
    let current = std::env::current_exe()?.canonicalize()?;
    let selected = if command == "nestweaver" {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| base.join(dir).join(command))
            .find(|path| path.is_file())
            .context("configured nestweaver is not available on PATH")?
    } else {
        ensure!(
            Path::new(command).is_absolute(),
            "custom MCP executable requires verification in the host; generic probe skipped"
        );
        Path::new(command).to_path_buf()
    };
    ensure!(
        selected.canonicalize()? == current,
        "configured executable is not this NestWeaver binary; generic probe skipped"
    );
    ensure!(
        args.first().is_some_and(|arg| arg == "mcp"),
        "only a canonical NestWeaver mcp invocation may be probed"
    );
    let mut seen = std::collections::HashSet::new();
    let mut at = 1;
    while at < args.len() {
        let flag = args[at].as_str();
        ensure!(
            seen.insert(flag),
            "duplicate MCP argument {flag}; generic probe skipped"
        );
        match flag {
            "--lite" | "--no-track-interactions" => at += 1,
            "--db" | "--config" | "--tools" => {
                let value = args.get(at + 1).context("MCP argument requires a value")?;
                ensure!(
                    !value.is_empty() && !value.starts_with('-'),
                    "invalid value for {flag}"
                );
                if flag == "--config" {
                    ensure!(
                        base.join(value).is_file(),
                        "configured MCP config file is unavailable"
                    );
                }
                at += 2;
            }
            _ => anyhow::bail!("non-allowlisted MCP argument {flag}; generic probe skipped"),
        }
    }
    ensure!(
        seen.contains("--db"),
        "canonical MCP probe requires an explicit database"
    );
    Ok(())
}

fn validated_registration(entry: &Value, base: &Path) -> Result<(String, Vec<String>)> {
    validate_probe_context(entry)?;
    let command = entry["command"]
        .as_str()
        .context("registration has no stdio command")?
        .to_string();
    let args = entry["args"]
        .as_array()
        .context("registration has no args array")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .context("non-string MCP argument")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut env = Vec::new();
    if let Some(value) = entry.get("env") {
        for (key, value) in value.as_object().context("MCP env is not an object")? {
            env.push((
                key.clone(),
                value
                    .as_str()
                    .context("non-string MCP environment value")?
                    .to_string(),
            ));
        }
    }
    let database = args
        .windows(2)
        .find(|pair| pair[0] == "--db")
        .map(|pair| base.join(&pair[1]))
        .context("registration has no explicit database; generic probe skipped")?;
    ensure!(
        database.is_file() && database.metadata()?.len() > 0,
        "index the configured database first, then run setup again"
    );
    validate_invocation(&command, &args, base, &env)?;
    let current = std::env::current_exe()?.canonicalize()?;
    let command = current
        .to_str()
        .context("NestWeaver executable path is not UTF-8; generic probe skipped")?
        .to_owned();
    Ok((command, args))
}

pub fn report(entry: &Value, base: &Path) {
    println!(
        "Host activation: unverified. Restart the host session and confirm NestWeaver tools are available."
    );
    println!(
        "Supervision: unknown/unverifiable (the MCP handshake does not report daemon ownership)."
    );
    let parsed = validated_registration(entry, base);
    let (command, args) = match parsed {
        Ok(value) => value,
        Err(error) => {
            println!("Server probe: not run ({error:#}).");
            return;
        }
    };
    let base = base.to_path_buf();
    // Setup is also called inside the CLI's Tokio runtime; use a dedicated
    // thread instead of nesting Runtime::block_on on a runtime worker.
    let result = std::thread::spawn(move || -> Result<usize> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(probe(&command, &args, &base, &[], Duration::from_secs(10)))
    })
    .join();
    match result {
        Ok(Ok(count)) => println!("Server probe: passed initialize + tools/list ({count} tools)."),
        Ok(Err(error)) => eprintln!(
            "Server probe: failed ({error:#}). Configuration alone does not establish activation."
        ),
        Err(_) => {
            eprintln!("Server probe: failed to complete. Host activation remains unverified.")
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn host_only_context_is_not_silently_approximated() {
        let ordinary = json!({"command":"nestweaver","args":["mcp","--db","graph.lbug"],"env":{"MODE":"plain"}});
        assert!(validate_probe_context(&ordinary).is_ok());
        for (key, value) in [
            ("cwd", json!("elsewhere")),
            ("env_vars", json!(["TOKEN"])),
            ("envFile", json!(".env")),
            ("experimental_environment", json!({})),
            ("disabled", json!(true)),
            ("enabled", json!(false)),
        ] {
            let mut entry = ordinary.clone();
            entry[key] = value;
            assert!(validate_probe_context(&entry).is_err(), "ignored {key}");
        }
        let mut entry = ordinary;
        entry["args"][2] = json!("${workspaceFolder}/graph.lbug");
        assert!(validate_probe_context(&entry).is_err());
    }

    #[test]
    fn configured_wrapper_is_never_executed_even_with_an_existing_database() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let command = dir.path().join("nestweaver");
        std::fs::write(
            &command,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
        let db = dir.path().join("existing.lbug");
        std::fs::write(&db, "existing file").unwrap();
        let entry = json!({"command":command,"args":["mcp","--db",db]});
        assert!(validated_registration(&entry, dir.path()).is_err());
        report(&entry, dir.path());
        assert!(
            !marker.exists(),
            "setup executed a repository-controlled wrapper"
        );
        let alias = dir.path().join("wrapper-alias");
        std::os::unix::fs::symlink(&command, &alias).unwrap();
        let mut aliased = entry;
        aliased["command"] = json!(alias);
        assert!(validated_registration(&aliased, dir.path()).is_err());
    }

    #[test]
    fn canonical_probe_refuses_loader_environment_other_commands_and_ambiguous_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("existing.lbug");
        std::fs::write(&db, "existing file").unwrap();
        let entry =
            json!({"command":std::env::current_exe().unwrap(),"args":["mcp","--lite","--db",db]});
        assert!(validated_registration(&entry, dir.path()).is_ok());
        for (key, value) in [
            ("env", json!({"LD_PRELOAD":"./repository-loader.so"})),
            ("env", json!({"PATH":"."})),
            ("args", json!(["index", "--db", db])),
            ("args", json!(["mcp", "--db", db, "--db", db])),
            ("args", json!(["mcp", "--db", db, "--track-interactions"])),
            ("args", json!(["mcp", "--db", db, "--unknown"])),
        ] {
            let mut unsupported = entry.clone();
            unsupported[key] = value;
            assert!(validated_registration(&unsupported, dir.path()).is_err());
        }
        std::fs::write(&db, "").unwrap();
        assert!(
            validated_registration(&entry, dir.path()).is_err(),
            "probe must not initialize an empty file"
        );
    }

    fn mock_server(dir: &Path, initialize: &str, listed: &str) -> Vec<String> {
        let path = dir.join("server.sh");
        std::fs::write(&path, format!("read -r init\nprintf '%s\\n' '{initialize}'\nread -r notification\nread -r list\nprintf '%s\\n' '{listed}'\nexec sleep 20\n")).unwrap();
        vec![path.to_string_lossy().to_string(), "--lite".into()]
    }

    #[tokio::test]
    async fn validates_lite_handshake_and_rejects_protocol_or_payload_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let init = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{}}}});
        let tools: Vec<_> = [
            "brain_context",
            "brain_search",
            "brain_impact",
            "brain_status",
            "brain_guide",
            "detect_changes",
        ]
        .iter()
        .map(|name| json!({"name":name,"inputSchema":{"type":"object"}}))
        .collect();
        let list = json!({"jsonrpc":"2.0","id":2,"result":{"tools":tools}});
        let args = mock_server(dir.path(), &init.to_string(), &list.to_string());
        assert_eq!(
            probe("sh", &args, dir.path(), &[], Duration::from_secs(2))
                .await
                .unwrap(),
            6
        );
        let mut full_args = args.clone();
        full_args.retain(|arg| arg != "--lite");
        assert!(
            probe("sh", &full_args, dir.path(), &[], Duration::from_secs(2))
                .await
                .is_err(),
            "lite catalogue must not pass as full registration"
        );
        for (bad_init, bad_list) in [
            ("not-json".to_string(), list.to_string()),
            (
                init.to_string().replace(PROTOCOL, "1900-01-01"),
                list.to_string(),
            ),
            (
                init.to_string(),
                json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}}).to_string(),
            ),
            (
                init.to_string(),
                json!({"jsonrpc":"2.0","id":3,"result":{"tools":tools}}).to_string(),
            ),
            (
                init.to_string(),
                json!({"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"unsupported"}})
                    .to_string(),
            ),
        ] {
            let args = mock_server(dir.path(), &bad_init, &bad_list);
            assert!(
                probe("sh", &args, dir.path(), &[], Duration::from_secs(2))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn missing_binary_and_stalled_server_fail_within_deadline() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            probe(
                "/definitely/missing/nestweaver",
                &[],
                dir.path(),
                &[],
                Duration::from_millis(50)
            )
            .await
            .is_err()
        );
        // `exec` ensures the pipe owner is the process killed by the probe.
        let started = std::time::Instant::now();
        let error = probe(
            "sh",
            &["-c".into(), "exec sleep 20".into()],
            dir.path(),
            &[],
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

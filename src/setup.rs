use std::io::Write as IoWrite;
use std::path::Path;

const DEPRECATED_MCP_ARGS: &[&str] = &["--allow-mcp-add-sources"];

struct ToolSetup {
    name: &'static str,
    detected: bool,
}

/// Print the one-time "NestWeaver Setup" banner (title + separator + database
/// line). Hoisted out of `run_setup` so a per-tool loop (auto-setup) prints it
/// at most once per invocation instead of once per detected tool (nw-051).
fn print_setup_banner(db_path: &Path) {
    let db_str = db_path.to_string_lossy();
    println!("NestWeaver Setup");
    println!("{}", "─".repeat(40));
    if db_path.exists() {
        println!("Database: {} (exists)", db_str);
    } else {
        println!("Database: {} (will be created on first index)", db_str);
    }
    println!();
}

/// Configure a single tool by name. Does NOT print the banner — callers own
/// banner printing so it happens once per invocation (nw-051).
fn configure_tool(
    name: &str,
    db_path: &Path,
    allow_writes: bool,
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    match name {
        "claude-code" => setup_claude_code(db_path, allow_writes, force_overwrite, base)?,
        "cursor" => setup_cursor(db_path, allow_writes, force_overwrite, base)?,
        "codex" => setup_codex(db_path, allow_writes, base)?,
        "windsurf" => setup_windsurf(db_path, allow_writes)?,
        "jetbrains" => setup_jetbrains(db_path, allow_writes, base)?,
        "vscode" => setup_vscode(db_path, allow_writes, base)?,
        "gemini" => setup_gemini(db_path, allow_writes, base)?,
        "copilot" => setup_copilot(db_path, allow_writes, base)?,
        "aider" => setup_aider(db_path, allow_writes, base)?,
        "kiro" => setup_kiro(db_path, allow_writes, base)?,
        "continue" => setup_continue(db_path, allow_writes, base)?,
        "cline" => setup_cline(db_path, allow_writes, base)?,
        "opencode" => setup_opencode(db_path, allow_writes, base)?,
        "trae" => setup_trae(db_path, allow_writes, base)?,
        "devin" => setup_devin(db_path, allow_writes, base)?,
        "hermes" => setup_hermes(db_path, allow_writes, base)?,
        _ => {}
    }
    Ok(())
}

pub fn run_setup(
    tool: Option<&str>,
    db_path: &Path,
    force_all: bool,
    allow_writes: bool,
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    print_setup_banner(db_path);

    let tools = detect_tools(base);

    let mut any_configured = false;
    for t in &tools {
        if let Some(specific) = tool
            && t.name != specific
        {
            continue;
        }

        if !t.detected && !force_all {
            println!("✗ {} — not detected", format_name(t.name));
            continue;
        }

        any_configured = true;
        configure_tool(t.name, db_path, allow_writes, force_overwrite, base)?;
    }

    if let Some(specific) = tool
        && !any_configured
    {
        anyhow::bail!(
            "tool '{}' not found; valid options: claude-code, cursor, codex, windsurf, jetbrains, vscode, gemini, copilot, aider, kiro, continue, cline, opencode, trae, devin, hermes",
            specific
        );
    }

    println!();
    if !db_path.exists() {
        println!("Run `nestweaver index --repo .` to index this repository.");
    }

    Ok(())
}

fn detect_tools(base: &Path) -> Vec<ToolSetup> {
    vec![
        ToolSetup {
            name: "claude-code",
            detected: base.join(".claude").exists() || which_exists("claude"),
        },
        ToolSetup {
            name: "cursor",
            detected: base.join(".cursor").exists(),
        },
        ToolSetup {
            name: "codex",
            detected: which_exists("codex"),
        },
        ToolSetup {
            name: "windsurf",
            detected: dirs::home_dir().is_some_and(|h| h.join(".codeium").exists()),
        },
        ToolSetup {
            name: "jetbrains",
            detected: base.join(".idea").exists() || base.join(".junie").exists(),
        },
        ToolSetup {
            name: "vscode",
            detected: base.join(".vscode").exists(),
        },
        ToolSetup {
            name: "gemini",
            detected: base.join(".gemini").exists() || which_exists("gemini"),
        },
        ToolSetup {
            name: "copilot",
            detected: base.join(".github/copilot-mcp.json").exists()
                || base.join(".github/copilot-instructions.md").exists()
                || which_exists("gh"),
        },
        ToolSetup {
            name: "aider",
            detected: base.join(".aider.conf.yml").exists() || which_exists("aider"),
        },
        ToolSetup {
            name: "kiro",
            detected: base.join(".kiro").exists() || which_exists("kiro"),
        },
        ToolSetup {
            name: "continue",
            detected: base.join(".continue").exists(),
        },
        ToolSetup {
            name: "cline",
            detected: base.join(".cline").exists(),
        },
        ToolSetup {
            name: "opencode",
            detected: base.join(".opencode").exists() || which_exists("opencode"),
        },
        ToolSetup {
            name: "trae",
            detected: base.join(".trae").exists(),
        },
        ToolSetup {
            name: "devin",
            detected: base.join("devin.json").exists() || which_exists("devin"),
        },
        ToolSetup {
            name: "hermes",
            detected: base.join(".hermes").exists() || which_exists("hermes"),
        },
    ]
}

fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ── Per-tool setup ────────────────────────────────────────────────────────────

fn mcp_args(db_str: &str, _allow_writes: bool) -> serde_json::Value {
    let args = vec!["mcp".to_string(), "--db".to_string(), db_str.to_string()];
    serde_json::json!(args)
}

fn mcp_args_lite(db_str: &str, _allow_writes: bool) -> serde_json::Value {
    let args = vec![
        "mcp".to_string(),
        "--lite".to_string(),
        "--db".to_string(),
        db_str.to_string(),
    ];
    serde_json::json!(args)
}

fn setup_claude_code(
    db_path: &Path,
    allow_writes: bool,
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".claude"))?;
    let mcp_path = base.join(".mcp.json");
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&mcp_path, "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(base.join(".claude/skills/nestweaver"))?;
    let skill_path = base.join(".claude/skills/nestweaver/SKILL.md");
    let skill_status = if skill_path.exists() && !force_overwrite {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&skill_path, generate_skill_content())?;
        "skill written"
    };

    // Install Claude Code hooks in .claude/settings.json
    let hooks_status = install_claude_hooks(&db_str, base)?;

    print_result(
        "Claude Code",
        &[
            (
                ".mcp.json",
                if merged {
                    "MCP server configured"
                } else {
                    "already configured"
                },
            ),
            (".claude/skills/nestweaver/SKILL.md", skill_status),
            (".claude/settings.json", hooks_status),
        ],
    );
    Ok(())
}

/// Install NestWeaver hooks into `.claude/settings.json`.
///
/// Adds a SessionStart hook that prints brain status so the agent knows
/// NestWeaver is available, and a PreToolUse hook for Bash that suggests
/// graph alternatives when the agent falls back to grep/find.
fn install_claude_hooks(db_str: &str, base: &Path) -> Result<&'static str, anyhow::Error> {
    let settings_path = base.join(".claude/settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    // Check if NestWeaver hooks already installed
    let hooks_obj = hooks.as_object_mut().unwrap();
    let already_installed = hooks_obj
        .get("SessionStart")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|c| c.as_str())
                                .map(|s| s.contains("nestweaver"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if already_installed {
        return Ok("hooks already installed");
    }

    // SessionStart: print brain status so the agent knows the graph is available
    let session_start = hooks_obj
        .entry("SessionStart")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = session_start.as_array_mut() {
        arr.push(serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": format!(
                    "nestweaver brain status --db {} 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'",
                    db_str
                )
            }]
        }));
    }

    // PreToolUse on Bash: when the agent runs grep/rg/find, suggest graph alternatives.
    // Non-blocking — returns additionalContext, never blocks the tool call.
    let pre_tool_use = hooks_obj
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = pre_tool_use.as_array_mut() {
        arr.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [{
                "type": "command",
                "command": format!(
                    "INPUT=$(cat); CMD=$(echo \"$INPUT\" | jq -r '.command // empty'); \
                     if echo \"$CMD\" | grep -qE '(grep|rg|find|fd|ack|ag)\\s'; then \
                       echo '{{\"additionalContext\": \"NestWeaver is indexed — prefer `brain_search` (searches code + notes) or `brain_context` (ranked structural context) over grep/find. Token savings: ~90% fewer tokens than file-by-file exploration.\"}}'; \
                     fi",
                )
            }]
        }));
    }

    std::fs::create_dir_all(base.join(".claude"))?;
    let formatted = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&settings_path, formatted)?;

    Ok("hooks installed")
}

fn setup_cursor(
    db_path: &Path,
    allow_writes: bool,
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".cursor"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args_lite(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".cursor/mcp.json"), "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(base.join(".cursor/rules"))?;
    let rule_path = base.join(".cursor/rules/nestweaver.mdc");
    let rule_status = if rule_path.exists() && !force_overwrite {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&rule_path, generate_cursor_rule_content())?;
        "agent rules written"
    };

    print_result(
        "Cursor",
        &[
            (
                ".cursor/mcp.json",
                if merged {
                    "MCP server (lite: 6 tools)"
                } else {
                    "already configured"
                },
            ),
            (".cursor/rules/nestweaver.mdc", rule_status),
        ],
    );
    Ok(())
}

fn setup_codex(db_path: &Path, _allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".codex"))?;
    let db_str = db_path.to_string_lossy();
    let config_path = base.join(".codex/config.toml");
    let toml_section = format!(
        "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"{}\"]\n",
        db_str
    );
    let merged = append_toml_if_missing(&config_path, "mcp_servers.nestweaver", &toml_section)?;

    let agents_path = base.join("AGENTS.md");
    let agents_status = if agents_path.exists() {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&agents_path, generate_agents_md_content())?;
        "codebase guide written"
    };

    print_result(
        "Codex",
        &[
            (
                ".codex/config.toml",
                if merged {
                    "MCP server configured"
                } else {
                    "already configured"
                },
            ),
            ("AGENTS.md", agents_status),
        ],
    );
    Ok(())
}

fn setup_windsurf(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let config_path = home.join(".codeium/windsurf/mcp_config.json");

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&config_path, "nestweaver", &mcp_config)?;

    print_result(
        "Windsurf",
        &[(
            &config_path.to_string_lossy(),
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_jetbrains(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".junie/mcp"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".junie/mcp/mcp.json"), "nestweaver", &mcp_config)?;

    print_result(
        "JetBrains",
        &[(
            ".junie/mcp/mcp.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_vscode(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".vscode"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".vscode/mcp.json"), "nestweaver", &mcp_config)?;

    print_result(
        "VS Code",
        &[(
            ".vscode/mcp.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_gemini(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".gemini"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        &base.join(".gemini/settings.json"),
        "nestweaver",
        &mcp_config,
    )?;

    print_result(
        "Gemini CLI",
        &[(
            ".gemini/settings.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_copilot(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".github"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        &base.join(".github/copilot-mcp.json"),
        "nestweaver",
        &mcp_config,
    )?;

    let instructions_path = base.join(".github/copilot-instructions.md");
    let instructions_status = if instructions_path.exists() {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&instructions_path, generate_copilot_instructions())?;
        "instructions written"
    };

    print_result(
        "GitHub Copilot",
        &[
            (
                ".github/copilot-mcp.json",
                if merged {
                    "MCP server configured"
                } else {
                    "already configured"
                },
            ),
            (".github/copilot-instructions.md", instructions_status),
        ],
    );
    Ok(())
}

fn setup_aider(db_path: &Path, _allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let config_path = base.join(".aider.conf.yml");
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let merged = if existing.contains("nestweaver") {
        false
    } else {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_path)?;
        file.write_all(
            format!(
                "\n# NestWeaver code intelligence\nrepo-map: nestweaver mcp --db {}\n",
                db_str
            )
            .as_bytes(),
        )?;
        true
    };

    print_result(
        "Aider",
        &[(
            ".aider.conf.yml",
            if merged {
                "repo-map reference configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_kiro(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".kiro"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".kiro/settings.json"), "nestweaver", &mcp_config)?;

    print_result(
        "Kiro",
        &[(
            ".kiro/settings.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_continue(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".continue"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        &base.join(".continue/config.json"),
        "nestweaver",
        &mcp_config,
    )?;

    print_result(
        "Continue.dev",
        &[(
            ".continue/config.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_cline(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".cline"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        &base.join(".cline/settings.json"),
        "nestweaver",
        &mcp_config,
    )?;

    print_result(
        "Cline",
        &[(
            ".cline/settings.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_opencode(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".opencode"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        &base.join(".opencode/config.json"),
        "nestweaver",
        &mcp_config,
    )?;

    print_result(
        "OpenCode",
        &[(
            ".opencode/config.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_trae(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".trae"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".trae/config.json"), "nestweaver", &mcp_config)?;

    print_result(
        "Trae",
        &[(
            ".trae/config.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_devin(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join("devin.json"), "nestweaver", &mcp_config)?;

    print_result(
        "Devin",
        &[(
            "devin.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

fn setup_hermes(db_path: &Path, allow_writes: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".hermes"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(&base.join(".hermes/config.json"), "nestweaver", &mcp_config)?;

    print_result(
        "Hermes",
        &[(
            ".hermes/config.json",
            if merged {
                "MCP server configured"
            } else {
                "already configured"
            },
        )],
    );
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Merge an MCP server entry into a JSON config file under the `mcpServers` key.
/// Returns `true` if the entry was newly added, `false` if it already existed.
fn merge_json_mcp(
    path: &Path,
    server_name: &str,
    config: &serde_json::Value,
) -> Result<bool, anyhow::Error> {
    let mut root = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                anyhow::bail!(
                    "{} contains invalid JSON: {}. Fix it manually or delete it.",
                    path.display(),
                    e
                );
            }
        }
    } else {
        serde_json::json!({})
    };

    {
        let servers = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("config at {} is not a JSON object", path.display()))?
            .entry("mcpServers")
            .or_insert(serde_json::json!({}));

        if servers.get(server_name).is_none() {
            servers
                .as_object_mut()
                .ok_or_else(|| {
                    anyhow::anyhow!("mcpServers is not an object in {}", path.display())
                })?
                .insert(server_name.to_string(), config.clone());

            // Drop borrow of root before serializing
            let json = serde_json::to_string_pretty(&root)?;
            std::fs::write(path, json)?;
            return Ok(true);
        }
    }

    // Server already exists — check for and strip deprecated args.
    let mut stripped: Vec<String> = Vec::new();
    {
        let root_obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("config at {} is not a JSON object", path.display()))?;
        if let Some(server_entry) = root_obj
            .get_mut("mcpServers")
            .and_then(|s| s.as_object_mut())
            .and_then(|s| s.get_mut(server_name))
            && let Some(args) = server_entry.get_mut("args").and_then(|a| a.as_array_mut())
        {
            args.retain(|arg| {
                let arg_str = arg.as_str().unwrap_or("");
                let is_deprecated = DEPRECATED_MCP_ARGS.contains(&arg_str);
                if is_deprecated {
                    stripped.push(arg_str.to_string());
                }
                !is_deprecated
            });
        }
    }

    if !stripped.is_empty() {
        let json = serde_json::to_string_pretty(&root)?;
        std::fs::write(path, json)?;
        for flag in &stripped {
            eprintln!("  (stripped deprecated flag: {flag})");
        }
    }

    Ok(false)
}

/// Append a TOML section to a file only if the section marker is not already present.
/// Returns `true` if the content was appended, `false` if it was already there.
fn append_toml_if_missing(
    path: &Path,
    section: &str,
    content: &str,
) -> Result<bool, anyhow::Error> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(section) {
        return Ok(false);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    Ok(true)
}

// ── Content generators ────────────────────────────────────────────────────────

fn generate_skill_content() -> String {
    // Build tool documentation dynamically from the MCP tool registry so
    // newly added tools automatically appear in the generated SKILL.md.
    let entries = nestweaver_mcp::tools::tool_doc_entries();
    let tool_docs: Vec<nestweaver_engine::ToolDocEntry> = entries
        .into_iter()
        .map(
            |(name, category, purpose, key_params)| nestweaver_engine::ToolDocEntry {
                name,
                category,
                purpose,
                key_params,
            },
        )
        .collect();

    // We don't have a GraphStore here (setup runs before indexing), so use
    // an in-memory store. The skill content doesn't depend on indexed data —
    // only tool metadata and static prose.
    let store = nestweaver_store::GraphStore::in_memory()
        .expect("in-memory GraphStore should always succeed");
    nestweaver_engine::generate_skill_with_tools(&store, None, None, &tool_docs)
        .expect("generate_skill_with_tools should not fail on empty store")
}

fn generate_cursor_rule_content() -> String {
    "---\ndescription: Use NestWeaver for structural codebase understanding\nglobs:\nalwaysApply: true\n---\n\n\
## Retrieval doctrine (token efficiency)\n\n\
**Prefer the graph over raw files.** A single `brain_context` call returns ~1,000 tokens of ranked, structural \
context vs ~10,000+ tokens from file-by-file exploration (validated 10x reduction, 2x fewer tool calls). Use NestWeaver tools INSTEAD OF grep/find/cat \
whenever you need to understand code structure, find related symbols, or check impact.\n\n\
- DO: `brain_context` seeded with a symbol → get ranked neighbors in one call\n\
- DO: `brain_search` to find symbols/notes by name → faster than grep, searches code AND notes\n\
- DO: `brain_impact` before modifying code → see blast radius without reading callers\n\
- DO NOT: grep/rg across the whole repo to find usages — `brain_context` already has them\n\
- DO NOT: read files to understand architecture — `brain_guide` or `hub_nodes` gives the structural picture\n\
- DO NOT: open files just to check what a function does — `read_symbols` returns just the symbol body\n\n\
## Key tools\n\n\
- `brain_context` — PPR-ranked structural context from symbol/note seeds\n\
- `brain_search` — full-text search across code AND notes in one call\n\
- `brain_impact` — blast radius before modifying code\n\
- `project_context` — project-scoped notes and symbols\n\
- `detect_changes` — assess risk after changes\n\
- `investigate` → `investigate_hydrate` → `read_symbols` — progressive disclosure\n\
- `dead_code` — find unreachable symbols\n\
- `hub_nodes` / `bridge_nodes` — find central and critical code\n\
- `get_summary` — token-efficient overview at file or cluster level\n\n\
## Quick Tool Reference\n\n\
- Explore a topic: `brain_context` (seed with name, filter by repo)\n\
- Find a symbol: `brain_search`\n\
- Check impact: `brain_impact` or `blast_radius`\n\
- Read source: `read_symbols` (not whole files)\n\
- Don't grep indexed repos — use `brain_search`\n\
- Don't read entire files — use `read_symbols`\n\n".to_string()
}

fn generate_copilot_instructions() -> String {
    let entries = nestweaver_mcp::tools::tool_doc_entries();
    let tool_count = entries.len();

    let mut out = format!(
        "# Copilot Instructions — NestWeaver\n\n\
    > Auto-generated by NestWeaver. Provides codebase intelligence via MCP.\n\n\
    ## Available MCP Tools ({tool_count})\n\n"
    );

    // Group by category for the copilot instructions
    let category_order = [
        "Core retrieval",
        "Analysis",
        "Investigation",
        "Code search",
        "Status & maintenance",
        "Extensions",
        "Vault health",
        "Memory",
    ];
    type DocEntry = (String, String, String, Vec<String>);
    let mut by_category: std::collections::BTreeMap<String, Vec<&DocEntry>> =
        std::collections::BTreeMap::new();
    for entry in &entries {
        by_category.entry(entry.1.clone()).or_default().push(entry);
    }

    for category in &category_order {
        if let Some(tools) = by_category.get(*category) {
            out.push_str(&format!("    ### {category}\n"));
            for (name, _cat, purpose, _params) in tools {
                let short: String = purpose.chars().take(80).collect();
                out.push_str(&format!("    - **{name}** — {short}\n"));
            }
            out.push('\n');
        }
    }

    out.push_str(
        "    ## When to Use\n\n\
    - Starting a task: call `brain_context` with task keywords\n\
    - Before modifying code: call `brain_impact` on the function\n\
    - Exploring unfamiliar code: call `brain_search`\n\
    - Finding dead code: call `dead_code` for unreachable symbols\n\
    - Architecture overview: call `hub_nodes`, `clusters`, and `get_summary`\n\n\
    ## Quick Tool Reference\n\n\
    - Explore a topic: `brain_context` (seed with name, filter by repo)\n\
    - Find a symbol: `brain_search`\n\
    - Check impact: `brain_impact` or `blast_radius`\n\
    - Read source: `read_symbols` (not whole files)\n\
    - Don't grep indexed repos — use `brain_search`\n\
    - Don't read entire files — use `read_symbols`\n\n\
    ## Interaction Memory\n\n\
    When the MCP server is started with `--track-interactions`, NestWeaver learns from agent \
    query patterns to improve retrieval ranking over time. Opt-in, local-only, records UIDs \
    and timestamps only — no content is captured. Use `interactions status` to view memory stats \
    and `interactions clear` to wipe interaction data.\n",
    );
    out
}

fn generate_agents_md_content() -> String {
    let entries = nestweaver_mcp::tools::tool_doc_entries();
    let tool_count = entries.len();

    let mut out = format!(
        "# AGENTS.md — Codebase Intelligence Guide\n\n\
> Auto-generated by NestWeaver. This file helps AI agents understand the codebase structure.\n\
> NestWeaver provides {tool_count} MCP tools for code intelligence. Run `nestweaver mcp` to start the server.\n\
> Run `nestweaver setup` to configure MCP for your AI tool.\n\n\
## Agent Playbook\n\n\
| I want to... | Use this |\n\
|---|---|\n\
| Understand a topic or module | `brain_context` seeded with the name, filtered by repo |\n\
| Find where something is defined | `brain_search` |\n\
| Check impact before changing code | `brain_impact` or `blast_radius` |\n\
| Trace execution from a function | `flow_trace` |\n\
| Read a symbol's source code | `read_symbols` (not Read on the whole file) |\n\
| Read a vault note | `note_get` |\n\
| Explore unfamiliar code | `investigate` then `investigate_expand` |\n\
| Find which tests to run | `affected_tests` |\n\
| Check if the index is current | `stale_check` |\n\
| Search by regex | `regex_search` (not grep) |\n\
| See architectural hotspots | `hub_nodes` or `bridge_nodes` |\n\
| Find module boundaries | `clusters` |\n\n\
## Available Tools\n\n\
| Tool | Description |\n\
|------|-------------|\n"
    );

    for (name, _category, purpose, _key_params) in &entries {
        // Take just the first sentence for the table
        let short: String = purpose.chars().take(80).collect();
        out.push_str(&format!("| {name} | {short} |\n"));
    }
    out
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn print_result(tool_name: &str, items: &[(&str, &str)]) {
    println!("✓ {tool_name}");
    for (path, desc) in items {
        println!("  • {path} — {desc}");
    }
}

fn format_name(name: &str) -> &str {
    match name {
        "claude-code" => "Claude Code",
        "cursor" => "Cursor",
        "codex" => "Codex",
        "windsurf" => "Windsurf",
        "jetbrains" => "JetBrains",
        "vscode" => "VS Code",
        "gemini" => "Gemini CLI",
        "copilot" => "GitHub Copilot",
        "aider" => "Aider",
        "kiro" => "Kiro",
        "continue" => "Continue.dev",
        "cline" => "Cline",
        "opencode" => "OpenCode",
        "trae" => "Trae",
        "devin" => "Devin",
        "hermes" => "Hermes",
        _ => name,
    }
}

/// Check if a tool already has NestWeaver configuration in the current directory.
/// Uses the primary config file that each tool's setup function writes.
fn tool_already_configured(tool_name: &str, base: &Path) -> bool {
    fn json_has_nestweaver(path: &Path) -> bool {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("mcpServers")
                    .and_then(|s| s.get("nestweaver"))
                    .map(|_| true)
            })
            .unwrap_or(false)
    }
    fn file_contains(path: &Path, needle: &str) -> bool {
        std::fs::read_to_string(path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
    }
    match tool_name {
        "claude-code" => json_has_nestweaver(&base.join(".mcp.json")),
        "cursor" => json_has_nestweaver(&base.join(".cursor/mcp.json")),
        "codex" => file_contains(&base.join(".codex/config.toml"), "nestweaver"),
        "windsurf" => dirs::home_dir()
            .map(|h| json_has_nestweaver(&h.join(".codeium/windsurf/mcp_config.json")))
            .unwrap_or(false),
        "jetbrains" => json_has_nestweaver(&base.join(".junie/mcp/mcp.json")),
        "vscode" => json_has_nestweaver(&base.join(".vscode/mcp.json")),
        "gemini" => json_has_nestweaver(&base.join(".gemini/settings.json")),
        "copilot" => json_has_nestweaver(&base.join(".github/copilot-mcp.json")),
        "aider" => file_contains(&base.join(".aider.conf.yml"), "nestweaver"),
        "kiro" => json_has_nestweaver(&base.join(".kiro/settings.json")),
        "continue" => json_has_nestweaver(&base.join(".continue/config.json")),
        "cline" => json_has_nestweaver(&base.join(".cline/settings.json")),
        "opencode" => json_has_nestweaver(&base.join(".opencode/config.json")),
        "trae" => json_has_nestweaver(&base.join(".trae/config.json")),
        "devin" => json_has_nestweaver(&base.join("devin.json")),
        "hermes" => json_has_nestweaver(&base.join(".hermes/config.json")),
        _ => false,
    }
}

/// Run setup automatically after first index. Only configures tools that are
/// detected and don't already have NestWeaver configs. Non-fatal — errors are
/// logged but don't propagate.
pub fn run_auto_setup(
    db_path: &std::path::Path,
    base: &Path,
    quiet: bool,
) -> Result<(), anyhow::Error> {
    // Only tools that are actually present and not yet wired up. Computed up
    // front so we can print the banner at most once — and skip it entirely when
    // there's nothing to configure (nw-051).
    let to_configure: Vec<&'static str> = detect_tools(base)
        .into_iter()
        .filter(|t| t.detected && !tool_already_configured(t.name, base))
        .map(|t| t.name)
        .collect();
    if to_configure.is_empty() {
        return Ok(());
    }

    // Banner once per invocation, and suppressed under --quiet (nw-051).
    if !quiet {
        print_setup_banner(db_path);
    }

    let mut configured = Vec::new();
    for name in to_configure {
        match configure_tool(name, db_path, false, false, base) {
            Ok(()) => configured.push(name),
            Err(e) => tracing::debug!("auto-setup skipped {}: {e}", name),
        }
    }

    if !configured.is_empty() && !quiet {
        eprintln!(
            "  Auto-configured NestWeaver for: {}. Run `nestweaver setup --help` to customize.",
            configured.join(", ")
        );
    }
    Ok(())
}

/// nw-023: auto-setup may only fire for the persona it was built for —
/// a human at a terminal, not suppressed by --quiet, indexing the repo
/// they are standing in. Pure so it is unit-testable without a pty.
/// `Path::starts_with` is component-wise, so /home/u/repo2 does not
/// count as inside /home/u/repo.
// Wired into the `Index` command handler in a later nw-023 task; the
// `#[cfg(test)]` gate below exercises it in the meantime.
pub fn should_auto_setup(
    stderr_is_tty: bool,
    quiet: bool,
    cwd: &std::path::Path,
    repo_root: &std::path::Path,
) -> bool {
    stderr_is_tty && !quiet && cwd.starts_with(repo_root)
}

#[cfg(test)]
mod auto_setup_gate_tests {
    use super::should_auto_setup;
    use std::path::Path;

    #[test]
    fn allows_tty_interactive_cwd_inside_repo() {
        assert!(should_auto_setup(
            true,
            false,
            Path::new("/home/u/repo/subdir"),
            Path::new("/home/u/repo")
        ));
    }
    #[test]
    fn allows_cwd_equal_to_repo_root() {
        assert!(should_auto_setup(
            true,
            false,
            Path::new("/r"),
            Path::new("/r")
        ));
    }
    #[test]
    fn blocks_when_stderr_not_a_tty() {
        assert!(!should_auto_setup(
            false,
            false,
            Path::new("/r"),
            Path::new("/r")
        ));
    }
    #[test]
    fn blocks_when_quiet() {
        assert!(!should_auto_setup(
            true,
            true,
            Path::new("/r"),
            Path::new("/r")
        ));
    }
    #[test]
    fn blocks_when_cwd_outside_repo() {
        assert!(!should_auto_setup(
            true,
            false,
            Path::new("/somewhere/else"),
            Path::new("/home/u/repo")
        ));
    }
    #[test]
    fn blocks_prefix_lookalike_dir() {
        // /home/u/repo2 must NOT count as inside /home/u/repo
        assert!(!should_auto_setup(
            true,
            false,
            Path::new("/home/u/repo2"),
            Path::new("/home/u/repo")
        ));
    }
}

#[cfg(test)]
mod setup_base_dir_tests {
    use super::*;

    /// The strong safety net: running setup with an explicit base must write
    /// ONLY under that base, never into the process cwd.
    #[test]
    fn run_setup_writes_only_under_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(base.join(".cursor")).unwrap(); // deterministic detection, no `which` reliance
        let db = tmp.path().join("t.lbug");
        std::fs::write(&db, "").unwrap();

        run_setup(Some("cursor"), &db, false, false, false, &base).unwrap();

        assert!(
            base.join(".cursor/mcp.json").exists(),
            "config must land under base"
        );
        assert!(base.join(".cursor/rules/nestweaver.mdc").exists());
    }

    #[test]
    fn detect_tools_probes_dot_dirs_under_base() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let tools = detect_tools(tmp.path());
        assert!(tools.iter().any(|t| t.name == "cursor" && t.detected));
    }
}

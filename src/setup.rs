use std::io::Write as IoWrite;
use std::path::Path;

const DEPRECATED_MCP_ARGS: &[&str] = &["--allow-mcp-add-sources"];

struct ToolSetup {
    name: &'static str,
    detected: bool,
}

pub fn run_setup(
    tool: Option<&str>,
    db_path: &Path,
    force_all: bool,
    allow_writes: bool,
    force_overwrite: bool,
) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();

    println!("NestWeaver Setup");
    println!("{}", "─".repeat(40));

    if db_path.exists() {
        println!("Database: {} (exists)", db_str);
    } else {
        println!("Database: {} (will be created on first index)", db_str);
    }
    println!();

    let tools = detect_tools();

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
        match t.name {
            "claude-code" => setup_claude_code(db_path, allow_writes, force_overwrite)?,
            "cursor" => setup_cursor(db_path, allow_writes, force_overwrite)?,
            "codex" => setup_codex(db_path, allow_writes)?,
            "windsurf" => setup_windsurf(db_path, allow_writes)?,
            "jetbrains" => setup_jetbrains(db_path, allow_writes)?,
            "vscode" => setup_vscode(db_path, allow_writes)?,
            "gemini" => setup_gemini(db_path, allow_writes)?,
            "copilot" => setup_copilot(db_path, allow_writes)?,
            "aider" => setup_aider(db_path, allow_writes)?,
            "kiro" => setup_kiro(db_path, allow_writes)?,
            "continue" => setup_continue(db_path, allow_writes)?,
            "cline" => setup_cline(db_path, allow_writes)?,
            "opencode" => setup_opencode(db_path, allow_writes)?,
            "trae" => setup_trae(db_path, allow_writes)?,
            "devin" => setup_devin(db_path, allow_writes)?,
            "hermes" => setup_hermes(db_path, allow_writes)?,
            _ => {}
        }
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

fn detect_tools() -> Vec<ToolSetup> {
    vec![
        ToolSetup {
            name: "claude-code",
            detected: Path::new(".claude").exists() || which_exists("claude"),
        },
        ToolSetup {
            name: "cursor",
            detected: Path::new(".cursor").exists(),
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
            detected: Path::new(".idea").exists() || Path::new(".junie").exists(),
        },
        ToolSetup {
            name: "vscode",
            detected: Path::new(".vscode").exists(),
        },
        ToolSetup {
            name: "gemini",
            detected: Path::new(".gemini").exists() || which_exists("gemini"),
        },
        ToolSetup {
            name: "copilot",
            detected: Path::new(".github/copilot-mcp.json").exists()
                || Path::new(".github/copilot-instructions.md").exists()
                || which_exists("gh"),
        },
        ToolSetup {
            name: "aider",
            detected: Path::new(".aider.conf.yml").exists() || which_exists("aider"),
        },
        ToolSetup {
            name: "kiro",
            detected: Path::new(".kiro").exists() || which_exists("kiro"),
        },
        ToolSetup {
            name: "continue",
            detected: Path::new(".continue").exists(),
        },
        ToolSetup {
            name: "cline",
            detected: Path::new(".cline").exists(),
        },
        ToolSetup {
            name: "opencode",
            detected: Path::new(".opencode").exists() || which_exists("opencode"),
        },
        ToolSetup {
            name: "trae",
            detected: Path::new(".trae").exists(),
        },
        ToolSetup {
            name: "devin",
            detected: Path::new("devin.json").exists() || which_exists("devin"),
        },
        ToolSetup {
            name: "hermes",
            detected: Path::new(".hermes").exists() || which_exists("hermes"),
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
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".claude")?;
    let mcp_path = Path::new(".mcp.json");
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(mcp_path, "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(".claude/skills/nestweaver")?;
    let skill_path = Path::new(".claude/skills/nestweaver/SKILL.md");
    let skill_status = if skill_path.exists() && !force_overwrite {
        "already exists (not overwritten)"
    } else {
        std::fs::write(skill_path, generate_skill_content())?;
        "skill written"
    };

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
        ],
    );
    Ok(())
}

fn setup_cursor(
    db_path: &Path,
    allow_writes: bool,
    force_overwrite: bool,
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".cursor")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args_lite(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".cursor/mcp.json"), "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(".cursor/rules")?;
    let rule_path = Path::new(".cursor/rules/nestweaver.mdc");
    let rule_status = if rule_path.exists() && !force_overwrite {
        "already exists (not overwritten)"
    } else {
        std::fs::write(rule_path, generate_cursor_rule_content())?;
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

fn setup_codex(db_path: &Path, _allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".codex")?;
    let db_str = db_path.to_string_lossy();
    let config_path = Path::new(".codex/config.toml");
    let toml_section = format!(
        "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"{}\"]\n",
        db_str
    );
    let merged = append_toml_if_missing(config_path, "mcp_servers.nestweaver", &toml_section)?;

    let agents_path = Path::new("AGENTS.md");
    let agents_status = if agents_path.exists() {
        "already exists (not overwritten)"
    } else {
        std::fs::write(agents_path, generate_agents_md_content())?;
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

fn setup_jetbrains(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".junie/mcp")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".junie/mcp/mcp.json"), "nestweaver", &mcp_config)?;

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

fn setup_vscode(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".vscode")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".vscode/mcp.json"), "nestweaver", &mcp_config)?;

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

fn setup_gemini(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".gemini")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        Path::new(".gemini/settings.json"),
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

fn setup_copilot(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".github")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        Path::new(".github/copilot-mcp.json"),
        "nestweaver",
        &mcp_config,
    )?;

    let instructions_path = Path::new(".github/copilot-instructions.md");
    let instructions_status = if instructions_path.exists() {
        "already exists (not overwritten)"
    } else {
        std::fs::write(instructions_path, generate_copilot_instructions())?;
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

fn setup_aider(db_path: &Path, _allow_writes: bool) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let config_path = Path::new(".aider.conf.yml");
    let existing = std::fs::read_to_string(config_path).unwrap_or_default();
    let merged = if existing.contains("nestweaver") {
        false
    } else {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config_path)?;
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

fn setup_kiro(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".kiro")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".kiro/settings.json"), "nestweaver", &mcp_config)?;

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

fn setup_continue(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".continue")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        Path::new(".continue/config.json"),
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

fn setup_cline(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".cline")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".cline/settings.json"), "nestweaver", &mcp_config)?;

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

fn setup_opencode(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".opencode")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(
        Path::new(".opencode/config.json"),
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

fn setup_trae(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".trae")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".trae/config.json"), "nestweaver", &mcp_config)?;

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

fn setup_devin(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new("devin.json"), "nestweaver", &mcp_config)?;

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

fn setup_hermes(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".hermes")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
    });
    let merged = merge_json_mcp(Path::new(".hermes/config.json"), "nestweaver", &mcp_config)?;

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
When exploring unfamiliar code, use the `brain_context` MCP tool with relevant symbol names as seeds.\n\n\
Before modifying functions with many callers, use `brain_impact` to check blast radius.\n\n\
For architecture questions, use `brain_guide` for a narrative overview, or `hub_nodes` and `bridge_nodes` to find the most connected and most critical nodes.\n\n\
For token-efficient overviews, use `get_summary` at file or cluster level instead of reading entire files.\n\n\
When working on a named project, use `project_context`.\n\n\
After making changes, use `detect_changes` to assess risk.\n\n\
To find cleanup opportunities, use `dead_code` to detect unreachable symbols.\n\n\
For deep-dive exploration, use `investigate` → `investigate_hydrate` → `read_symbols` (only where `body_complete` is false). This progressive disclosure pattern avoids reading every file upfront.\n\n\
Prefer `brain_search` over `brain_context` when locating symbols or notes by name — it returns both notes and code symbols in one call.\n\n\
If using many MCP servers, pass `--tools` to the NestWeaver server to allowlist only the tools you need.\n".to_string()
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

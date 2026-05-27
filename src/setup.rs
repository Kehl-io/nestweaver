use std::io::Write as IoWrite;
use std::path::Path;

struct ToolSetup {
    name: &'static str,
    detected: bool,
}

pub fn run_setup(tool: Option<&str>, db_path: &Path, force_all: bool) -> Result<(), anyhow::Error> {
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
            "claude-code" => setup_claude_code(db_path)?,
            "cursor" => setup_cursor(db_path)?,
            "codex" => setup_codex(db_path)?,
            "windsurf" => setup_windsurf(db_path)?,
            "jetbrains" => setup_jetbrains(db_path)?,
            "vscode" => setup_vscode(db_path)?,
            _ => {}
        }
    }

    if let Some(specific) = tool
        && !any_configured
    {
        anyhow::bail!(
            "tool '{}' not found; valid options: claude-code, cursor, codex, windsurf, jetbrains, vscode",
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

fn setup_claude_code(db_path: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".claude")?;
    let settings_path = Path::new(".claude/settings.json");
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": ["mcp", "--db", db_str.as_ref()]
    });
    let merged = merge_json_mcp(settings_path, "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(".claude/skills/nestweaver")?;
    std::fs::write(
        ".claude/skills/nestweaver/SKILL.md",
        generate_skill_content(),
    )?;

    print_result(
        "Claude Code",
        &[
            (
                ".claude/settings.json",
                if merged {
                    "MCP server configured"
                } else {
                    "already configured"
                },
            ),
            (".claude/skills/nestweaver/SKILL.md", "skill written"),
        ],
    );
    Ok(())
}

fn setup_cursor(db_path: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".cursor")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": ["mcp", "--lite", "--db", db_str.as_ref()]
    });
    let merged = merge_json_mcp(Path::new(".cursor/mcp.json"), "nestweaver", &mcp_config)?;

    std::fs::create_dir_all(".cursor/rules")?;
    std::fs::write(
        ".cursor/rules/nestweaver.mdc",
        generate_cursor_rule_content(),
    )?;

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
            (".cursor/rules/nestweaver.mdc", "agent rules written"),
        ],
    );
    Ok(())
}

fn setup_codex(db_path: &Path) -> Result<(), anyhow::Error> {
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

fn setup_windsurf(db_path: &Path) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let config_path = home.join(".codeium/windsurf/mcp_config.json");

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": ["mcp", "--db", db_str.as_ref()]
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

fn setup_jetbrains(db_path: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".junie/mcp")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": ["mcp", "--db", db_str.as_ref()]
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

fn setup_vscode(db_path: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".vscode")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": ["mcp", "--db", db_str.as_ref()]
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

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config at {} is not a JSON object", path.display()))?
        .entry("mcpServers")
        .or_insert(serde_json::json!({}));

    if servers.get(server_name).is_some() {
        return Ok(false); // already configured
    }

    servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers is not an object in {}", path.display()))?
        .insert(server_name.to_string(), config.clone());

    let json = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, json)?;
    Ok(true)
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
    "---\nname: nestweaver\ndescription: Use when exploring code architecture, checking blast radius, understanding dependencies, or working with vault notes\n---\n\n\
## When to use NestWeaver\n\n\
- **Starting a task**: Call `brain_context` with task keywords\n\
- **Before modifying code**: Call `brain_impact` on the function\n\
- **Exploring unfamiliar code**: Call `brain_search`\n\
- **Working on a project**: Call `project_context`\n\n\
## Tools\n\n\
| Tool | Use when |\n\
|------|----------|\n\
| brain_context | You need structural context for a task |\n\
| brain_search | You need to find symbols or notes |\n\
| brain_impact | You need to check blast radius |\n\
| brain_guide | You need an architecture overview |\n\
| project_context | You're working on a named project |\n\
| detect_changes | You want to assess risk of changes |\n".to_string()
}

fn generate_cursor_rule_content() -> String {
    "---\ndescription: Use NestWeaver for structural codebase understanding\nglobs:\nalwaysApply: true\n---\n\n\
When exploring unfamiliar code, use the `brain_context` MCP tool with relevant symbol names as seeds.\n\n\
Before modifying functions with many callers, use `brain_impact` to check blast radius.\n\n\
For architecture questions, use `brain_guide`.\n\n\
When working on a named project, use `project_context`.\n\n\
After making changes, use `detect_changes` to assess risk.\n".to_string()
}

fn generate_agents_md_content() -> String {
    "# AGENTS.md — Codebase Intelligence Guide\n\n\
> Auto-generated by NestWeaver. This file helps AI agents understand the codebase structure.\n\
> NestWeaver provides 17 MCP tools for code intelligence. Run `nestweaver mcp` to start the server.\n\
> Run `nestweaver setup` to configure MCP for your AI tool.\n\n\
## Available Tools\n\n\
| Tool | Description |\n\
|------|-------------|\n\
| brain_context | PPR-ranked context for a task |\n\
| brain_search | Full-text search across code and notes |\n\
| brain_impact | Blast radius analysis |\n\
| brain_guide | Architecture overview |\n\
| project_context | Project-scoped retrieval |\n\
| detect_changes | Risk assessment for changes |\n\
| brain_status | Index status and staleness |\n\
| stale_check | Check if re-indexing is needed |\n\
| flow_trace | Execution flow tracing |\n\
| note_get | Retrieve vault notes |\n\
| backlinks | Find notes linking to a target |\n\
| brain_diff | Graph change detection |\n\
| clusters | Community detection results |\n\
| cross_repo_contracts | Cross-repo relationships |\n\
| brain_add_source | Index new sources at runtime |\n\
| set_extension | Attach custom metadata |\n\
| query_extensions | Query custom metadata |\n".to_string()
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
        _ => name,
    }
}

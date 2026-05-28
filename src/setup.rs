use std::io::Write as IoWrite;
use std::path::Path;

struct ToolSetup {
    name: &'static str,
    detected: bool,
}

pub fn run_setup(
    tool: Option<&str>,
    db_path: &Path,
    force_all: bool,
    allow_writes: bool,
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
            "claude-code" => setup_claude_code(db_path, allow_writes)?,
            "cursor" => setup_cursor(db_path, allow_writes)?,
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

fn mcp_args(db_str: &str, allow_writes: bool) -> serde_json::Value {
    let mut args = vec!["mcp".to_string(), "--db".to_string(), db_str.to_string()];
    if allow_writes {
        args.push("--allow-mcp-add-sources".to_string());
    }
    serde_json::json!(args)
}

fn mcp_args_lite(db_str: &str, allow_writes: bool) -> serde_json::Value {
    let mut args = vec![
        "mcp".to_string(),
        "--lite".to_string(),
        "--db".to_string(),
        db_str.to_string(),
    ];
    if allow_writes {
        args.push("--allow-mcp-add-sources".to_string());
    }
    serde_json::json!(args)
}

fn setup_claude_code(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".claude")?;
    let settings_path = Path::new(".claude/settings.json");
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, allow_writes)
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

fn setup_cursor(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".cursor")?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args_lite(&db_str, allow_writes)
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

fn setup_codex(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(".codex")?;
    let db_str = db_path.to_string_lossy();
    let config_path = Path::new(".codex/config.toml");
    let allow_writes_arg = if allow_writes {
        ", \"--allow-mcp-add-sources\"".to_string()
    } else {
        String::new()
    };
    let toml_section = format!(
        "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"{}\"{}]\n",
        db_str, allow_writes_arg
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

fn setup_aider(db_path: &Path, allow_writes: bool) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let config_path = Path::new(".aider.conf.yml");
    let existing = std::fs::read_to_string(config_path).unwrap_or_default();
    let merged = if existing.contains("nestweaver") {
        false
    } else {
        let allow_writes_arg = if allow_writes {
            " --allow-mcp-add-sources"
        } else {
            ""
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config_path)?;
        file.write_all(
            format!(
                "\n# NestWeaver code intelligence\nrepo-map: nestweaver mcp --db {}{}\n",
                db_str, allow_writes_arg
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
    "\
---
name: nestweaver
description: |
  Use when exploring codebase structure, understanding dependencies, analyzing
  change impact, navigating architecture, or retrieving knowledge-vault notes.
  Do NOT use for simple text search, file reading, or tasks unrelated to code
  structure and project knowledge.
---

## When to activate

Activate this skill when the task involves:

- Understanding how a function, module, or file fits into the codebase
- Checking what will break before modifying a symbol (blast radius)
- Tracing call chains or execution flow from an entry point
- Navigating cross-repository dependencies or shared contracts
- Retrieving project knowledge from Obsidian vaults or markdown notes
- Getting a structural overview of the architecture
- Assessing risk of a set of changed files before commit
- Detecting dead or unused code
- Finding architectural hotspots or chokepoints

Do NOT activate when:

- The user just wants to read a specific file (use normal file reading)
- The task is plain text search with no structural intent (use grep/ripgrep)
- The question is about runtime behavior, logs, or deployment

## Key concepts

- **Seeds**: Starting points for a graph walk. Can be symbol names, note titles, tag names (with or without `#`), free-text terms, or UIDs (`sym:`, `note:`, `head:`, `sec:`, `tag:`).
- **PPR (Personalized PageRank)**: Walks the code+notes graph from seeds and scores every reachable node by structural proximity.
- **Intent**: The `intent` parameter on `brain_context`/`project_context` tunes PPR edge weights for specific query types (e.g. `find-definition`, `find-callers`).
- **Context**: A token-budgeted, PPR-ranked list of symbols, notes, and sections relevant to given seeds.
- **Brain**: The unified graph combining code symbols and markdown vault notes.
- **Vault**: An indexed collection of markdown notes (e.g. an Obsidian vault). Use `.brainignore` for glob exclusion patterns.
- **Edge types**: CALLS (function calls), IMPORTS, USES (type references), ACCESSES (field access). PPR weights each differently.
- **Confidence**: A 0.0\u{2013}1.0 score on edges indicating resolver certainty about a relationship.

## Available MCP tools

### Core retrieval

| Tool | Purpose |
|------|---------|
| `brain_context` | PPR-ranked context from seeds. **Call this first** for any structural question. Supports `intent` parameter. |
| `brain_search` | BM25 full-text search across notes, headings, sections, and tags. |
| `project_context` | PPR-ranked context scoped to a named project. Supports `intent` parameter. |
| `note_get` | Full markdown body of a specific note. |
| `backlinks` | All notes that wikilink TO a target note. |
| `get_summary` | Hierarchical code summaries at symbol, file, or cluster level. Token-efficient overview. |

### Analysis

| Tool | Purpose |
|------|---------|
| `brain_impact` | Blast radius: all symbols that call/import/extend the target, grouped by depth. |
| `flow_trace` | Forward call chain from a symbol. |
| `detect_changes` | Risk assessment for a list of changed files. |
| `blast_radius` | Analyze blast radius of a symbol change with risk scoring. |
| `dead_code` | Detect unreachable symbols via entry point reachability analysis. |
| `hub_nodes` | Most connected hub nodes by degree centrality and PageRank. |
| `bridge_nodes` | Architectural chokepoints by betweenness centrality. |
| `cross_repo_contracts` | Symbols shared across repositories. |
| `clusters` | Functional communities detected by the Leiden algorithm. |

### Status and maintenance

| Tool | Purpose |
|------|---------|
| `brain_status` | Counts of vaults, notes, symbols, repos. |
| `stale_check` | Compare indexed SHA to current git HEAD. |
| `brain_diff` | Files and symbols changed since a given SHA. |
| `brain_guide` | Auto-generated architecture overview. |
| `brain_add_source` | Index a new repo or vault at runtime. |
| `set_extension` | Attach custom metadata to graph nodes. |
| `query_extensions` | Query custom metadata on graph nodes. |

## Common workflows

### Understanding a function

1. `brain_context` with the function name as a seed.
2. `flow_trace` for forward call chain.
3. `brain_impact` for callers/dependents.

### Before modifying code

1. `brain_impact` on the symbol you plan to change.
2. `detect_changes` with the list of files you expect to modify.
3. `cross_repo_contracts` if the symbol may be shared across services.

### Assessing dead code

1. `dead_code` to find unreachable symbols across the codebase.
2. `brain_context` on flagged symbols to verify they are truly unused.
3. Remove confirmed dead code with confidence.

### Architecture overview

1. `hub_nodes` to identify the most connected symbols.
2. `bridge_nodes` to find architectural chokepoints.
3. `clusters` to see functional groupings.
4. `get_summary` at cluster or file level for a token-efficient overview.
"
    .to_string()
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
If using many MCP servers, pass `--tools` to the NestWeaver server to allowlist only the tools you need.\n".to_string()
}

fn generate_copilot_instructions() -> String {
    "# Copilot Instructions — NestWeaver\n\n\
    > Auto-generated by NestWeaver. Provides codebase intelligence via MCP.\n\n\
    ## Available MCP Tools (22)\n\n\
    ### Core retrieval\n\
    - **brain_context** — PPR-ranked context from seeds (supports `intent` parameter)\n\
    - **brain_search** — BM25 full-text search across code and notes\n\
    - **project_context** — Project-scoped PPR context (supports `intent` parameter)\n\
    - **note_get** — Full markdown body of a specific note\n\
    - **backlinks** — Notes that wikilink to a target note\n\
    - **get_summary** — Hierarchical code summaries (symbol/file/cluster level)\n\n\
    ### Analysis\n\
    - **brain_impact** — Reverse dependency blast radius, grouped by depth\n\
    - **flow_trace** — Forward call chain from a symbol\n\
    - **detect_changes** — Risk assessment for a list of changed files\n\
    - **blast_radius** — Symbol change blast radius with risk scoring\n\
    - **dead_code** — Detect unreachable symbols via entry point reachability\n\
    - **hub_nodes** — Most connected nodes by degree centrality and PageRank\n\
    - **bridge_nodes** — Architectural chokepoints by betweenness centrality\n\
    - **cross_repo_contracts** — Symbols shared across repositories\n\
    - **clusters** — Functional communities (Leiden algorithm)\n\n\
    ### Status and maintenance\n\
    - **brain_status** — Vault, note, symbol, and repo counts\n\
    - **stale_check** — Compare indexed SHA to current git HEAD\n\
    - **brain_diff** — Files and symbols changed since a given SHA\n\
    - **brain_guide** — Auto-generated architecture overview\n\
    - **brain_add_source** — Index a new repo or vault at runtime\n\
    - **set_extension** — Attach custom metadata to graph nodes\n\
    - **query_extensions** — Query custom metadata on graph nodes\n\n\
    ## When to Use\n\n\
    - Starting a task: call `brain_context` with task keywords\n\
    - Before modifying code: call `brain_impact` on the function\n\
    - Exploring unfamiliar code: call `brain_search`\n\
    - Finding dead code: call `dead_code` for unreachable symbols\n\
    - Architecture overview: call `hub_nodes`, `clusters`, and `get_summary`\n"
        .to_string()
}

fn generate_agents_md_content() -> String {
    "# AGENTS.md — Codebase Intelligence Guide\n\n\
> Auto-generated by NestWeaver. This file helps AI agents understand the codebase structure.\n\
> NestWeaver provides 22 MCP tools for code intelligence. Run `nestweaver mcp` to start the server.\n\
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
| query_extensions | Query custom metadata |\n\
| dead_code | Detect unreachable symbols via entry point reachability |\n\
| hub_nodes | Show most connected hub nodes by degree centrality |\n\
| bridge_nodes | Show architectural bridge/chokepoint nodes |\n\
| blast_radius | Analyze blast radius of a symbol change |\n\
| get_summary | Retrieve hierarchical code summaries |\n".to_string()
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

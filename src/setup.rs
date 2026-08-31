use anyhow::Context as _;
use nestweaver_engine::user_config;
use std::io::Write as IoWrite;
use std::path::Path;

const DEPRECATED_MCP_ARGS: &[&str] = &["--allow-mcp-add-sources"];

/// The command every refusal below tells the user to re-run. A constant so the
/// message and the thing they type cannot drift apart.
const SETUP_COMMAND: &str = "`nestweaver setup`";

/// Refuse to rewrite a config file NestWeaver cannot rewrite without loss.
///
/// Every JSON config `setup` touches is a file the user owns and NestWeaver
/// merely adds one entry to. Two shapes make a serialize-and-replace write
/// destructive, and both are checked here so the sixteen per-tool writers share
/// one rule instead of sixteen near-misses:
///
/// * a **symlink** — a rename replaces the link, a plain write modifies
///   whatever it pointed at, and neither is a file `setup` was pointed at;
/// * **JSONC comments** — VS Code and Claude Code both accept them in these
///   files, and `serde_json` cannot round-trip one.
fn guard_user_owned_write(
    path: &Path,
    config: &user_config::JsonConfig,
) -> Result<(), anyhow::Error> {
    if config.is_symlink {
        return Err(user_config::refuse_symlink(
            path,
            &format!(
                "Add the `nestweaver` entry to the file the link points at, then \
                 run {SETUP_COMMAND} again."
            ),
        ));
    }
    if config.has_comments {
        return Err(user_config::refuse_comments(
            path,
            &format!(
                "Add the `nestweaver` entry by hand, or remove the comments from \
                 the file and run {SETUP_COMMAND} again."
            ),
        ));
    }
    Ok(())
}

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
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    match name {
        "claude-code" => setup_claude_code(db_path, force_overwrite, base)?,
        "cursor" => setup_cursor(db_path, force_overwrite, base)?,
        "codex" => setup_codex(db_path, base)?,
        "windsurf" => setup_windsurf(db_path, base)?,
        "jetbrains" => setup_jetbrains(db_path, base)?,
        "vscode" => setup_vscode(db_path, base)?,
        "gemini" => setup_gemini(db_path, base)?,
        "copilot" => setup_copilot(db_path, base)?,
        "aider" => setup_aider(db_path, base)?,
        "kiro" => setup_kiro(db_path, base)?,
        "continue" => setup_continue(db_path, base)?,
        "cline" => setup_cline(db_path, base)?,
        "opencode" => setup_opencode(db_path, base)?,
        "trae" => setup_trae(db_path, base)?,
        "devin" => setup_devin(db_path, base)?,
        "hermes" => setup_hermes(db_path, base)?,
        _ => {}
    }
    Ok(())
}

pub fn run_setup(
    tool: Option<&str>,
    db_path: &Path,
    force_all: bool,
    // Accepted for CLI compatibility and deliberately unused: `--allow-writes`
    // has had no effect on any generated registration for some time, and the
    // per-tool writers threaded it purely to discard it. Kept as a parameter so
    // the existing (hidden, deprecated) flag still parses rather than becoming
    // a hard error for anyone who still passes it.
    _allow_writes: bool,
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
        configure_tool(t.name, db_path, force_overwrite, base)?;
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

/// The instance config this database is bound to, if one is known.
///
/// nw-199: a generated `.mcp.json` used to carry only `mcp --db <db>`, so an
/// MCP server started from it could never see instance.toml — which made the
/// entire `mcp --config` surface (`[limits]`, `[response]`, `[ranking]`) dead
/// through the supported install path. Any policy that lives in config was
/// therefore unreachable, and hand-adding a flag to a generated file was
/// pointless because the next `setup` run regenerates it.
///
/// This asks the same question the daemon asks on a configless start: what
/// config was last successfully used for this database? Reusing the daemon's
/// own persisted intent means setup cannot bind a config the daemon would
/// disagree with, and it needs no new flag to thread through every caller.
fn bound_config_path(db_path: &Path, base: &Path) -> Result<Option<String>, anyhow::Error> {
    // 1. What the daemon actually last used for this database. Authoritative
    //    when present, because setup then cannot bind a config the daemon would
    //    disagree with.
    if let Ok(record) = nestweaver_daemon::lifecycle::read_last_successful_config(db_path)
        && Path::new(&record.config_path).is_file()
    {
        nestweaver_engine::InstanceConfig::from_file(Path::new(&record.config_path)).with_context(
            || {
                format!(
                    "validate the persisted NestWeaver config binding {}",
                    record.config_path
                )
            },
        )?;
        return Ok(Some(record.config_path));
    }

    // 2. Fall back to discovery from the directory being configured.
    //
    // The daemon record only exists AFTER a daemon has successfully started
    // with a `--config`. On a genuinely fresh install — `setup` run before any
    // index, which is the order the install docs describe — there is no record,
    // so relying on it alone silently emitted a config-less registration in the
    // exact case that matters most. Look where an instance config actually
    // lives relative to the tree being set up.
    // `.nestweaver/` is our namespace. A file there is explicit operator
    // intent, so accepting defaults after it fails to parse would silently
    // discard configuration the user expected us to honor.
    let explicit_candidate = base.join(".nestweaver/instance.toml");
    if explicit_candidate.is_file() {
        nestweaver_engine::InstanceConfig::from_file(&explicit_candidate).with_context(|| {
            format!(
                "{} is the NestWeaver instance config for this setup, but it is invalid",
                explicit_candidate.display()
            )
        })?;
        return explicit_candidate
            .canonicalize()
            .with_context(|| format!("canonicalize config path {}", explicit_candidate.display()))
            .map(|path| Some(path.display().to_string()));
    }

    // A bare `instance.toml` is only a compatibility heuristic and may belong
    // to another tool. Use it when it parses as a complete NestWeaver config;
    // otherwise disclose the skipped candidate and continue without binding
    // it instead of making an unrelated file break setup.
    let bare_candidate = base.join("instance.toml");
    if bare_candidate.is_file() {
        match nestweaver_engine::InstanceConfig::from_file(&bare_candidate) {
            Ok(_) => {
                return bare_candidate
                    .canonicalize()
                    .with_context(|| {
                        format!("canonicalize config path {}", bare_candidate.display())
                    })
                    .map(|path| Some(path.display().to_string()));
            }
            Err(error) => eprintln!(
                "warning: ignoring bare config candidate {} because it is not a valid \
                 NestWeaver instance config: {error:#}",
                bare_candidate.display()
            ),
        }
    }
    Ok(None)
}

/// THE single source of truth for the argv every generated MCP registration
/// gets. Every per-tool writer must go through this.
///
/// It is a function rather than three literals because the duplication is what
/// caused the bug: `--config` was added to the JSON writers while the Codex
/// TOML writer kept its own hardcoded `["mcp", "--db", "{}"]` literal, so Codex
/// silently kept emitting a config-less registration. `generated_registrations_
/// all_carry_the_bound_config` pins that they cannot drift apart again.
fn mcp_arg_vec(db_str: &str, base: &Path, lite: bool) -> Result<Vec<String>, anyhow::Error> {
    let mut args = vec!["mcp".to_string()];
    if lite {
        args.push("--lite".to_string());
    }
    args.push("--db".to_string());
    args.push(db_str.to_string());
    if let Some(config) = bound_config_path(Path::new(db_str), base)? {
        args.push("--config".to_string());
        args.push(config);
    }
    Ok(args)
}

fn mcp_args(db_str: &str, base: &Path) -> Result<serde_json::Value, anyhow::Error> {
    Ok(serde_json::json!(mcp_arg_vec(db_str, base, false)?))
}

fn mcp_args_lite(db_str: &str, base: &Path) -> Result<serde_json::Value, anyhow::Error> {
    Ok(serde_json::json!(mcp_arg_vec(db_str, base, true)?))
}

fn setup_claude_code(
    db_path: &Path,
    force_overwrite: bool,
    base: &Path,
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".claude"))?;
    let mcp_path = base.join(".mcp.json");
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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
    // `unwrap_or_else(|_| json!({}))` here was silent DATA LOSS. A settings
    // file with a trailing comma, a merge-conflict marker, or a half-saved
    // edit parsed as an error, became an empty object, and was then written
    // back containing only NestWeaver's hooks — destroying the user's
    // permissions, env, model config and every other hook they had. The
    // command then reported "hooks installed".
    //
    // `merge_json_mcp` — the same operation on the same kind of file, 500
    // lines below in this file — already refuses and says why. This is that
    // behaviour, so the two agree.
    //
    // The refusal, the JSONC read, the symlink check and the atomic write are
    // now shared with `nestweaver admin install-hook` — which had this defect
    // in its original form and destroyed a live API key with it — via
    // `nestweaver_engine::user_config`. Valid JSON that is NOT an object (`[]`,
    // `"text"`, `null`) used to reach `.as_object_mut().unwrap()` and panic;
    // that too is a refusal there now, for the same reason: whatever the file
    // is, it is the user's.
    let existing = user_config::read_json_config(&settings_path, SETUP_COMMAND)?;
    let mut settings = existing.value.clone();
    // Unreachable: `read_json_config` has already refused anything that is not
    // an object. Kept as a `bail!` rather than an `unwrap` so a future change to
    // that guarantee surfaces as an error instead of a panic in a command that
    // is holding the user's settings open.
    let Some(root) = settings.as_object_mut() else {
        anyhow::bail!(
            "{} did not read back as a JSON object",
            settings_path.display(),
        );
    };
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));

    // Check if NestWeaver hooks already installed
    let Some(hooks_obj) = hooks.as_object_mut() else {
        anyhow::bail!(
            "{} has a \"hooks\" key that is not an object. Fix it manually or \
             remove that key.",
            settings_path.display()
        );
    };
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

    // Idempotency before the write guards: there is nothing to write, so there
    // is nothing to refuse, and a repeat run succeeds on a file this function
    // would decline to rewrite.
    if already_installed {
        return Ok("hooks already installed");
    }
    guard_user_owned_write(&settings_path, &existing)?;

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
                // Not a `format!`: this string interpolates nothing, and a
                // newer clippy rejects `useless_format` under -D warnings. The
                // `{}` here are literal JSON braces, so they are NOT doubled —
                // doubling only escapes them for a format string.
                "command": "INPUT=$(cat); CMD=$(echo \"$INPUT\" | jq -r '.command // empty'); \
                     if echo \"$CMD\" | grep -qE '(grep|rg|find|fd|ack|ag)\\s'; then \
                       echo '{\"additionalContext\": \"NestWeaver is indexed — prefer `brain_search` (searches code + notes) or `brain_context` (ranked structural context) over grep/find. Token savings: ~90% fewer tokens than file-by-file exploration.\"}'; \
                     fi"
            }]
        }));
    }

    std::fs::create_dir_all(base.join(".claude"))?;
    let formatted = serde_json::to_string_pretty(&settings)?;
    user_config::replace_file_atomically(&settings_path, &formatted)?;

    Ok("hooks installed")
}

fn setup_cursor(db_path: &Path, force_overwrite: bool, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".cursor"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args_lite(&db_str, base)?
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

fn setup_codex(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".codex"))?;
    let db_str = db_path.to_string_lossy();
    let config_path = base.join(".codex/config.toml");
    let toml_section = format!(
        "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [{}]\n",
        mcp_arg_vec(&db_str, base, false)?
            .iter()
            .map(|arg| format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let merged = merge_codex_mcp(&config_path, &toml_section)?;

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

fn setup_windsurf(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let config_path = home.join(".codeium/windsurf/mcp_config.json");

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_jetbrains(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".junie/mcp"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_vscode(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".vscode"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_gemini(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".gemini"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_copilot(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".github"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_aider(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
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

fn setup_kiro(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".kiro"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_continue(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".continue"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_cline(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".cline"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_opencode(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".opencode"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_trae(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".trae"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_devin(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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

fn setup_hermes(db_path: &Path, base: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(base.join(".hermes"))?;
    let db_str = db_path.to_string_lossy();
    let mcp_config = serde_json::json!({
        "command": "nestweaver",
        "args": mcp_args(&db_str, base)?
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
    // The canonical argv is already in `config`; read the desired `--config`
    // out of it rather than threading it through every per-tool caller.
    let desired_config = config
        .get("args")
        .and_then(|a| a.as_array())
        .and_then(|args| {
            args.iter()
                .position(|a| a.as_str() == Some("--config"))
                .and_then(|i| args.get(i + 1))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    // `path.exists()` FOLLOWS SYMLINKS, so a symlinked `.mcp.json` was read
    // and written through to wherever it pointed — and a DANGLING one read as
    // "absent", sending this straight to the create branch, which then created
    // the link's target outside the project. `read_json_config` judges
    // existence with `symlink_metadata` and reports the link as a link.
    let existing = user_config::read_json_config(path, SETUP_COMMAND)?;
    let mut root = existing.value.clone();

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
            guard_user_owned_write(path, &existing)?;
            let json = serde_json::to_string_pretty(&root)?;
            user_config::replace_file_atomically(path, &json)?;
            return Ok(true);
        }
    }

    // Server already exists.
    //
    // Reconcile it rather than leaving it alone. This branch used to only PRUNE
    // deprecated flags, so a registration written before `--config` existed kept
    // its old argv forever: every upgrade path silently stayed config-less, and
    // re-running `setup` — the obvious remedy — changed nothing. Additive only,
    // so a hand-tuned `--lite`, `--tools` or a deliberately different `--db` is
    // preserved; the sole thing added is a `--config` the entry does not have.
    let mut stripped: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
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
            let has_config = args.iter().any(|arg| arg.as_str() == Some("--config"));
            if !has_config && let Some(config) = desired_config.as_deref() {
                args.push(serde_json::Value::String("--config".to_string()));
                args.push(serde_json::Value::String(config.to_string()));
                added.push("--config".to_string());
            }
        }
    }

    if !stripped.is_empty() || !added.is_empty() {
        guard_user_owned_write(path, &existing)?;
        let json = serde_json::to_string_pretty(&root)?;
        user_config::replace_file_atomically(path, &json)?;
        for flag in &stripped {
            eprintln!("  (stripped deprecated flag: {flag})");
        }
        for flag in &added {
            eprintln!("  (added missing flag: {flag})");
        }
    }

    Ok(false)
}

/// Merge the generated Codex MCP registration without replacing user-owned
/// arguments. Returns `true` when the section was added or reconciled.
fn merge_codex_mcp(path: &Path, content: &str) -> Result<bool, anyhow::Error> {
    let desired = content
        .parse::<toml_edit::DocumentMut>()
        .context("parse generated Codex MCP configuration")?;
    let desired_args = desired
        .get("mcp_servers")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|servers| servers.get("nestweaver"))
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|server| server.get("args"))
        .and_then(toml_edit::Item::as_array)
        .ok_or_else(|| anyhow::anyhow!("generated Codex MCP configuration has no args array"))?;
    let desired_config = desired_args
        .iter()
        .position(|value| value.as_str() == Some("--config"))
        .and_then(|index| desired_args.get(index + 1))
        .and_then(toml_edit::Value::as_str);

    // Codex's config is TOML and goes through `toml_edit`, which preserves
    // comments and formatting — so this file has no JSONC problem. It shares
    // the other two: `exists()` followed symlinks, and each `fs::write` below
    // truncated the user's `~/.codex/config.toml` in place.
    let probe = user_config::probe(path)?;
    if !probe.existed {
        user_config::replace_file_atomically(path, content)?;
        return Ok(true);
    }
    if probe.is_symlink {
        return Err(user_config::refuse_symlink(
            path,
            &format!(
                "Add the `[mcp_servers.nestweaver]` section to the file the link \
                 points at, then run {SETUP_COMMAND} again."
            ),
        ));
    }

    let existing = std::fs::read_to_string(path)?;
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{} contains invalid TOML", path.display()))?;
    let server_exists = document
        .get("mcp_servers")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|servers| servers.get("nestweaver"))
        .is_some();
    if !server_exists {
        let mut appended = existing;
        if !appended.ends_with('\n') {
            appended.push('\n');
        }
        appended.push_str(content.trim_start_matches('\n'));
        user_config::replace_file_atomically(path, &appended)?;
        return Ok(true);
    }

    let Some(desired_config) = desired_config else {
        return Ok(false);
    };
    let args = document
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|servers| servers.get_mut("nestweaver"))
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|server| server.get_mut("args"))
        .and_then(toml_edit::Item::as_array_mut)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[mcp_servers.nestweaver] in {} must contain an args array",
                path.display()
            )
        })?;

    if let Some(config_index) = args
        .iter()
        .position(|value| value.as_str() == Some("--config"))
    {
        if args
            .get(config_index + 1)
            .and_then(toml_edit::Value::as_str)
            .is_none()
        {
            anyhow::bail!(
                "[mcp_servers.nestweaver] in {} has --config without a path",
                path.display()
            );
        }
        return Ok(false);
    }

    // Add only the missing pair. `--tools`, `--lite`, a custom `--db`, comments,
    // and multiline formatting remain owned by the user.
    args.push("--config");
    args.push(desired_config);
    user_config::replace_file_atomically(path, &document.to_string())?;
    eprintln!("  (added missing flag: --config)");
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
    Set `[ranking] track_interactions = true` in your instance config (or start the MCP \
    server with `--track-interactions`) and NestWeaver learns from agent \
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

/// Run setup automatically after first index. Every detected tool is rerun
/// idempotently while the completion marker is absent so a retry can repair
/// secondary artifacts written after the primary config. Failures are returned
/// so the caller can leave the marker absent and retry later.
pub fn run_auto_setup(
    db_path: &std::path::Path,
    base: &Path,
    quiet: bool,
) -> Result<(), anyhow::Error> {
    // Compute detected tools up front so we can print the banner at most once
    // and skip it entirely when there's nothing to configure (nw-051).
    let to_configure: Vec<&'static str> = detect_tools(base)
        .into_iter()
        .filter(|t| t.detected)
        .map(|t| t.name)
        .collect();
    run_auto_setup_for_tools(db_path, base, quiet, &to_configure)
}

fn run_auto_setup_for_tools(
    db_path: &Path,
    base: &Path,
    quiet: bool,
    to_configure: &[&str],
) -> Result<(), anyhow::Error> {
    if to_configure.is_empty() {
        return Ok(());
    }

    // Banner once per invocation, and suppressed under --quiet (nw-051).
    if !quiet {
        print_setup_banner(db_path);
    }

    let mut configured = Vec::new();
    let mut failures = Vec::new();
    for &name in to_configure {
        match configure_tool(name, db_path, false, base) {
            Ok(()) => configured.push(name),
            Err(error) => failures.push(format!("{name}: {error:#}")),
        }
    }

    if !configured.is_empty() && !quiet {
        eprintln!(
            "  Auto-configured NestWeaver for: {}. Run `nestweaver setup --help` to customize.",
            configured.join(", ")
        );
    }
    if !failures.is_empty() {
        anyhow::bail!("auto-setup failed for {}", failures.join("; "));
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

    #[test]
    fn run_auto_setup_reports_partial_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(base.join(".cursor")).unwrap();
        std::fs::write(base.join(".cursor/mcp.json"), "{ invalid json").unwrap();
        let db = tmp.path().join("test.lbug");

        let error = run_auto_setup_for_tools(&db, &base, true, &["cursor"]).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("cursor"));
        // Wording tracks `user_config::read_json_config`, which now names the
        // parse position too — the refusal is shared with `admin install-hook`.
        assert!(message.contains("not valid JSON"), "{message}");
        assert!(message.contains("changed nothing"), "{message}");
    }

    #[test]
    fn run_auto_setup_retries_missing_secondary_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(base.join(".cursor")).unwrap();
        let rules_path = base.join(".cursor/rules");
        std::fs::write(&rules_path, "blocks directory creation").unwrap();
        let db = tmp.path().join("test.lbug");

        setup_cursor(&db, false, &base).unwrap_err();
        let mcp_path = base.join(".cursor/mcp.json");
        let primary_before_retry = std::fs::read_to_string(&mcp_path).unwrap();
        assert!(!rules_path.join("nestweaver.mdc").exists());

        std::fs::remove_file(&rules_path).unwrap();
        run_auto_setup_for_tools(&db, &base, true, &["cursor"]).unwrap();

        assert!(rules_path.join("nestweaver.mdc").exists());
        assert_eq!(
            std::fs::read_to_string(mcp_path).unwrap(),
            primary_before_retry
        );
    }
}

#[cfg(test)]
mod mcp_arg_source_of_truth_tests {
    use super::*;

    /// nw-199 follow-up, from a user report: `--config` was added to the JSON
    /// writers while the Codex writer kept its own hardcoded
    /// `["mcp", "--db", "{}"]` literal, so Codex alone kept emitting a
    /// config-less registration — the exact drift the single builder exists to
    /// prevent. Grep the source: no writer may hand-roll the argv.
    #[test]
    fn no_writer_hand_rolls_the_mcp_argv() {
        // Build the needle from pieces so this detector does not match its own
        // source line — the joined form never appears literally in this file.
        let needle = ["\"mcp\"", "\"--db\""].join(", ");
        let escaped = needle.replace('"', "\\\"");
        let source = include_str!("setup.rs");
        // Stop before this test module so the assertion message and the needle
        // pieces cannot themselves be flagged.
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(source);

        let mut offenders = Vec::new();
        for (n, line) in production.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains(&needle) || line.contains(&escaped) {
                offenders.push(format!("{}: {}", n + 1, line.trim()));
            }
        }
        assert!(
            offenders.is_empty(),
            "every generated MCP registration must come from `mcp_arg_vec`, so a flag \
             added once reaches every tool. Hand-rolled argv found:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Fresh first run: `setup` before any daemon has ever started must still
    /// find the instance config.
    ///
    /// `bound_config_path` originally consulted only the daemon's persisted
    /// binding, which does not exist until a daemon has successfully started
    /// with a `--config`. The install docs run `setup` BEFORE the first index,
    /// so the common case emitted a config-less registration.
    #[test]
    fn fresh_setup_discovers_the_config_without_a_daemon_binding() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join(".nestweaver")).unwrap();
        std::fs::write(
            base.join(".nestweaver/instance.toml"),
            include_str!("../examples/minimal-instance.toml"),
        )
        .unwrap();

        let db = base.join("never-started.lbug");
        let found = bound_config_path(&db, base)
            .expect("config discovery must succeed")
            .expect("config must be discovered from base");
        assert!(
            found.ends_with("instance.toml"),
            "expected the instance config, got {found}"
        );
        assert!(
            mcp_arg_vec(&db.display().to_string(), base, false)
                .unwrap()
                .iter()
                .any(|a| a == "--config"),
            "a fresh registration must carry --config"
        );
    }

    /// A config inside NestWeaver's own namespace is explicit operator intent.
    /// Setup must fail visibly instead of silently discarding an invalid file
    /// the user expected it to honor.
    #[test]
    fn fresh_setup_rejects_an_invalid_explicit_config() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join(".nestweaver")).unwrap();
        let config = base.join(".nestweaver/instance.toml");
        std::fs::write(&config, "title = \"not a NestWeaver config\"\n").unwrap();

        let error = bound_config_path(&base.join("fresh.lbug"), base).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&config.display().to_string()));
        assert!(message.contains("invalid"));
    }

    /// The root-level filename is ambiguous and may belong to another tool.
    /// An invalid candidate there is disclosed but must not break setup.
    #[test]
    fn fresh_setup_skips_an_invalid_bare_config_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("instance.toml"),
            "title = \"another tool owns this\"\n",
        )
        .unwrap();

        let found = bound_config_path(&base.join("fresh.lbug"), base)
            .expect("an unrelated bare config must not break setup");
        assert_eq!(found, None);
    }

    /// The compatibility filename remains supported when it proves itself by
    /// parsing as a complete NestWeaver instance config.
    #[test]
    fn fresh_setup_accepts_a_valid_bare_config_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let config = base.join("instance.toml");
        std::fs::write(&config, include_str!("../examples/minimal-instance.toml")).unwrap();

        let found = bound_config_path(&base.join("fresh.lbug"), base)
            .expect("config discovery must succeed")
            .expect("a valid bare config must remain discoverable");
        assert_eq!(found, config.canonicalize().unwrap().display().to_string());
    }

    /// Upgrade path: an EXISTING registration must gain `--config`.
    ///
    /// This branch used to only prune deprecated flags, so a config written
    /// before `--config` existed kept its old argv forever and re-running
    /// `setup` was a silent no-op.
    #[test]
    fn existing_json_registration_gains_the_missing_config_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"nestweaver":{"command":"nestweaver","args":["mcp","--db","/db"]}}}"#,
        )
        .unwrap();

        let desired = serde_json::json!({
            "command": "nestweaver",
            "args": ["mcp", "--db", "/db", "--config", "/cfg/instance.toml"]
        });
        merge_json_mcp(&path, "nestweaver", &desired).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let args = written["mcpServers"]["nestweaver"]["args"]
            .as_array()
            .unwrap();
        assert!(
            args.iter().any(|a| a == "--config"),
            "an existing registration must be reconciled, not left stale: {args:?}"
        );
        // Additive only — the original db argument survives untouched.
        assert!(args.iter().any(|a| a == "/db"));
    }

    /// Same upgrade path for the Codex TOML writer, which returned unchanged
    /// the moment the section existed.
    #[test]
    fn existing_codex_section_gains_the_missing_config_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp_servers.nestweaver]
command = "nestweaver"
args = [
    "mcp",
    "--lite",
    "--db",
    "/custom-db", # preserve this deliberate binding
    "--tools",
    "brain_search,brain_context",
]
"#,
        )
        .unwrap();

        let desired = "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"/db\", \"--config\", \"/cfg/instance.toml\"]\n";
        merge_codex_mcp(&path, desired).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("--config"),
            "an existing Codex section must be reconciled: {written}"
        );
        for preserved in [
            "--lite",
            "--tools",
            "brain_search,brain_context",
            "/custom-db",
            "preserve this deliberate binding",
        ] {
            assert!(
                written.contains(preserved),
                "reconciliation removed user-owned {preserved:?}: {written}"
            );
        }
        // Exactly one section — reconciled in place, not appended twice.
        assert_eq!(written.matches("[mcp_servers.nestweaver]").count(), 1);
    }

    /// Lite and full registrations differ only by `--lite`; both must carry the
    /// same db and (when one is bound) the same config.
    #[test]
    fn lite_and_full_argv_agree_except_for_the_lite_flag() {
        // A base with no instance config, so neither carries --config and the
        // comparison isolates the --lite difference.
        let empty = tempfile::tempdir().unwrap();
        let full = mcp_arg_vec("/tmp/does-not-exist.lbug", empty.path(), false).unwrap();
        let lite = mcp_arg_vec("/tmp/does-not-exist.lbug", empty.path(), true).unwrap();
        assert_eq!(full[0], "mcp");
        assert_eq!(lite[0], "mcp");
        assert_eq!(lite[1], "--lite");
        assert_eq!(&full[1..], &lite[2..]);
        assert!(full.contains(&"--db".to_string()));
    }
}

/// nw-212 / nw-217: `setup` must not destroy the file it is editing.
///
/// `install_claude_hooks` read `.claude/settings.json`, and on a parse error
/// substituted an empty object — then wrote that back containing only
/// NestWeaver's hooks. A trailing comma, a merge-conflict marker or a
/// half-saved edit therefore DISCARDED the user's permissions, env, model
/// config and every other hook, and the command reported "hooks installed".
///
/// `merge_json_mcp`, the same operation on the same kind of file 500 lines
/// above, already refused and said why. This is the sibling gap closed, and
/// pinned so it cannot reopen.
#[cfg(test)]
mod settings_preservation_tests {
    use super::*;

    fn claude_settings(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(".claude/settings.json")
    }

    fn write_settings(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = claude_settings(dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    /// The data-loss case. The file must come back BYTE-IDENTICAL — asserting
    /// only that the call failed would still pass if it errored after
    /// truncating.
    #[test]
    fn malformed_settings_are_refused_and_left_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        // A real trailing comma, the single most common way this file breaks,
        // wrapped around settings that matter.
        let original =
            "{\n  \"permissions\": { \"allow\": [\"Bash(ls:*)\"] },\n  \"model\": \"opus\",\n}\n";
        let path = write_settings(dir.path(), original);

        let result = install_claude_hooks("/tmp/x.lbug", dir.path());

        let error = result.expect_err("invalid JSON must be refused, not silently replaced");
        let message = error.to_string();
        assert!(
            message.contains("not valid JSON"),
            "the error must say what is wrong: {message}"
        );
        assert!(
            message.contains("line 4") && message.contains("column"),
            "and WHERE — a refusal that will not name the position leaves the \
             user to find it: {message}"
        );
        assert!(
            message.contains("settings.json"),
            "and which file: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the user's settings must be untouched — a refusal that still \
             truncates is the same data loss with a better error message"
        );
    }

    /// Valid JSON that is not an object reached `.as_object_mut().unwrap()` and
    /// PANICKED. A panic is not a refusal: it leaves the operator with a
    /// backtrace instead of an instruction.
    #[test]
    fn non_object_settings_are_refused_without_panicking() {
        for content in ["[1, 2, 3]", "\"just a string\"", "null", "42"] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_settings(dir.path(), content);

            let error = install_claude_hooks("/tmp/x.lbug", dir.path())
                .expect_err("non-object settings must be refused");

            assert!(
                error.to_string().contains("not an object"),
                "for {content:?} the error must name the problem: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                content,
                "for {content:?} the file must be untouched"
            );
        }
    }

    /// The half that keeps the fix honest: a VALID file must still be edited,
    /// and every key the user already had must survive. A guard that refused
    /// everything would pass both tests above.
    #[test]
    fn valid_settings_keep_every_existing_key_when_hooks_are_added() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(
            dir.path(),
            r#"{"permissions":{"allow":["Bash(ls:*)"]},"model":"opus","hooks":{"Stop":[{"matcher":"","hooks":[]}]}}"#,
        );

        install_claude_hooks("/tmp/x.lbug", dir.path()).expect("valid settings must be edited");

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["model"], serde_json::json!("opus"), "model dropped");
        assert_eq!(
            after["permissions"]["allow"][0],
            serde_json::json!("Bash(ls:*)"),
            "permissions dropped"
        );
        assert!(
            after["hooks"]["Stop"].is_array(),
            "an unrelated hook the user had was dropped: {after}"
        );
        assert!(
            after["hooks"]["SessionStart"].is_array(),
            "the NestWeaver hook was not actually installed: {after}"
        );
    }

    /// A missing file is not an error — that is the first-run path.
    #[test]
    fn absent_settings_are_created_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();

        install_claude_hooks("/tmp/x.lbug", dir.path()).expect("first run must succeed");

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(claude_settings(dir.path())).unwrap())
                .unwrap();
        assert!(after["hooks"]["SessionStart"].is_array());
    }
}

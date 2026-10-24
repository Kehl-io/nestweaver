use anyhow::Context as _;
use nestweaver_engine::user_config;
use std::path::Path;

const DEPRECATED_MCP_ARGS: &[&str] = &["--allow-mcp-add-sources"];

/// The command every refusal below tells the user to re-run. A constant so the
/// message and the thing they type cannot drift apart.
const SETUP_COMMAND: &str = "`nestweaver setup`";

/// Refuse to rewrite a config file NestWeaver cannot rewrite without loss.
///
/// Every JSON config `setup` touches is a file the user owns and NestWeaver
/// merely adds one entry to. One shape makes a serialize-and-replace write
/// destructive, and it is checked here so the sixteen per-tool writers share
/// one rule instead of sixteen near-misses: **JSONC comments** — VS Code and
/// Claude Code both accept them in these files, and `serde_json` cannot
/// round-trip one.
///
/// A **symlink** used to be refused here too. It no longer is: symlinking
/// `.mcp.json` or `.claude/settings.json` into a dotfiles repository is a
/// deliberate workflow, and `user_config::resolve_for_write` follows the link
/// so the write lands on the file it names and the link survives.
fn guard_user_owned_write(
    path: &Path,
    config: &user_config::JsonConfig,
) -> Result<(), anyhow::Error> {
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

/// Every tool name `setup` accepts, in the order they are attempted.
const KNOWN_TOOLS: &[&str] = &[
    "claude-code",
    "cursor",
    "codex",
    "windsurf",
    "jetbrains",
    "vscode",
    "gemini",
    "copilot",
    "aider",
    "kiro",
    "continue",
    "cline",
    "opencode",
    "trae",
    "devin",
    "hermes",
];

struct ToolSetup {
    name: &'static str,
    detected: bool,
}

/// What one `setup` run was asked to do. Threaded to every per-tool writer so
/// `--force` reaches the MCP registrations as well as the skill/rule files.
struct SetupCtx<'a> {
    /// Always absolute: a relative path in a registration resolves against
    /// whatever directory the host happens to launch the server from, and in a
    /// global config (Windsurf) that is not even this project.
    db: &'a Path,
    base: &'a Path,
    force: bool,
    /// Whether `--force` may rebind a GLOBAL registration (Windsurf's one
    /// per-user entry). Only when that tool was named explicitly: `setup --all
    /// --force` in project B must not silently take the entry from project A.
    force_global: bool,
    /// Run the live server probe after a successful tool. Off in unit tests,
    /// which must never launch whatever `nestweaver` is on PATH.
    probe: bool,
}

impl SetupCtx<'_> {
    fn db_str(&self) -> String {
        self.db.to_string_lossy().into_owned()
    }
}

/// `(file, what happened to it)` rows for one tool, accumulated as the writes
/// happen so a failure part-way through can still say what was written.
type Items = Vec<(String, String)>;

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

/// Resolve the database path every registration will carry to an absolute
/// path. `std::path::absolute` does not touch the filesystem, so a database
/// that does not exist yet (setup before the first index) still resolves.
fn absolute_db_path(db_path: &Path) -> Result<std::path::PathBuf, anyhow::Error> {
    std::path::absolute(db_path)
        .with_context(|| format!("resolve database path {}", db_path.display()))
}

/// Configure a single tool by name, recording each file it touches in
/// `items`. Does NOT print — callers own the output so a failure can be
/// reported together with what was already written (nw-051).
fn configure_tool(name: &str, ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    for relative in project_paths(name, ctx.base)? {
        refuse_directory_escape(ctx.base, &relative)?;
    }
    match name {
        "claude-code" => setup_claude_code(ctx, items),
        "cursor" => setup_cursor(ctx, items),
        "codex" => setup_codex(ctx, items),
        "windsurf" => setup_windsurf(ctx, items),
        "copilot" => setup_copilot(ctx, items),
        "aider" => setup_aider(ctx, items),
        "cline" => setup_cline(ctx, items),
        other => {
            let Some(target) = json_registration(other, ctx.base)? else {
                anyhow::bail!("unknown tool '{other}'");
            };
            if let Some(parent) = target.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            register_json(ctx, items, &target, false, "MCP server configured")?;
            Ok(())
        }
    }
}

/// The project-relative files `name` writes, for [`refuse_directory_escape`].
/// A global file (Windsurf's) is not under the project and is not listed.
fn project_paths(name: &str, base: &Path) -> Result<Vec<std::path::PathBuf>, anyhow::Error> {
    let fixed: &[&str] = match name {
        "claude-code" => &[
            ".claude/skills/nestweaver/SKILL.md",
            ".claude/settings.json",
        ],
        "cursor" => &[".cursor/rules/nestweaver.mdc"],
        "codex" => &[".codex/config.toml", "AGENTS.md"],
        "copilot" => &[".github/copilot-instructions.md"],
        "aider" => &[".aider.conf.yml"],
        _ => &[],
    };
    let mut paths: Vec<std::path::PathBuf> = fixed.iter().map(std::path::PathBuf::from).collect();
    if let Some(target) = json_registration(name, base)?
        && let Ok(relative) = target.path.strip_prefix(base)
    {
        paths.push(relative.to_path_buf());
    }
    Ok(paths)
}

/// Refuse when any component on the way to `relative` (below `base`) is a
/// symbolic link that resolves outside the project.
///
/// `create_dir_all` and the atomic write both follow directory links, so a
/// cloned repository shipping `.continue -> ~/.config/<app>` (or `.kiro`,
/// `.claude`, …) made `setup` — and the auto-setup `index` runs — create
/// directories and write a registration outside the project. A link that
/// stays inside the project is fine. Explicit global/user configuration writes
/// retain their dotfiles support in `user_config`; project setup cannot infer
/// consent to an external write from a link supplied by the repository.
fn refuse_directory_escape(base: &Path, relative: &Path) -> Result<(), anyhow::Error> {
    let canonical_base = base
        .canonicalize()
        .with_context(|| format!("resolve project directory {}", base.display()))?;
    let mut current = base.to_path_buf();
    // Include the final file: following an external dotfiles link is a write
    // outside the project just as following a directory link is.
    for component in relative.components() {
        current.push(component);
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            // Absent: everything below it will be created inside `current`'s
            // already-checked parent.
            return Ok(());
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let inside = current
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(&canonical_base));
        if !inside {
            anyhow::bail!(
                "{} is a symbolic link that leads outside the project ({}). NestWeaver \
                 changed nothing for this tool: it will not create directories or write \
                 files outside {} through a link. Replace the link with a directory, or \
                 add the registration by hand.",
                current.display(),
                std::fs::read_link(&current)
                    .map_or_else(|_| "unresolvable".to_string(), |t| t.display().to_string()),
                base.display(),
            );
        }
    }
    Ok(())
}

/// Where a tool's JSON MCP registration lives and under which key.
struct JsonRegistration {
    path: std::path::PathBuf,
    /// The row label printed for this file.
    label: String,
    /// `mcpServers` for almost every host; VS Code's `.vscode/mcp.json` uses
    /// `servers`, OpenCode's `opencode.json` uses `mcp`.
    servers_key: &'static str,
}

fn json_registration(name: &str, base: &Path) -> Result<Option<JsonRegistration>, anyhow::Error> {
    let (relative, servers_key) = match name {
        "claude-code" => (".mcp.json", "mcpServers"),
        "cursor" => (".cursor/mcp.json", "mcpServers"),
        "jetbrains" => (".junie/mcp/mcp.json", "mcpServers"),
        // VS Code's own workspace format is `{"servers": {...}}`; an
        // `mcpServers` key there is ignored. Copilot in VS Code reads the same
        // file — `.github/copilot-mcp.json`, which setup used to write, is not
        // read by any Copilot surface.
        "vscode" | "copilot" => (".vscode/mcp.json", "servers"),
        "gemini" => (".gemini/settings.json", "mcpServers"),
        // Kiro's workspace MCP file; `.kiro/settings.json` is not read for MCP.
        "kiro" => (".kiro/settings/mcp.json", "mcpServers"),
        // Continue reads standalone server files from `.continue/mcpServers/`
        // and accepts the Claude/Cursor JSON shape there. `.continue/config.json`
        // is its deprecated main config, not an MCP file.
        "continue" => (".continue/mcpServers/nestweaver.json", "mcpServers"),
        // OpenCode reads `opencode.json` at the project root, servers under
        // `mcp`, each `{"type": "local", "command": [argv...]}`. `.opencode/`
        // holds agents, commands and plugins only.
        "opencode" => ("opencode.json", "mcp"),
        "trae" => (".trae/config.json", "mcpServers"),
        "devin" => ("devin.json", "mcpServers"),
        "hermes" => (".hermes/config.json", "mcpServers"),
        "windsurf" => {
            let home = dirs::home_dir().context("cannot determine home dir")?;
            let path = home.join(".codeium/windsurf/mcp_config.json");
            return Ok(Some(JsonRegistration {
                label: path.display().to_string(),
                path,
                servers_key: "mcpServers",
            }));
        }
        _ => return Ok(None),
    };
    Ok(Some(JsonRegistration {
        path: base.join(relative),
        label: relative.to_string(),
        servers_key,
    }))
}

fn probe_configured_tool(name: &str, base: &Path) {
    // Aider has no MCP client and Cline has no project-level file: setup
    // writes no registration for either, so there is nothing to probe.
    if name != "aider" && name != "cline" {
        match configured_mcp_entry(name, base) {
            Ok(entry) => crate::setup_probe::report(&entry, base),
            Err(error) => eprintln!(
                "Server probe: not run ({error:#}). Host activation remains unverified; restart the host and inspect its tool list."
            ),
        }
    }
}

/// Read back what setup actually left on disk, including preserved custom
/// commands, profiles, database bindings, and environment variables.
fn configured_mcp_entry(name: &str, base: &Path) -> anyhow::Result<serde_json::Value> {
    if name == "codex" {
        let text = std::fs::read_to_string(base.join(".codex/config.toml"))?;
        let root: serde_json::Value = toml_edit::de::from_str(&text)?;
        return serde_json::to_value(
            root.get("mcp_servers")
                .and_then(|v| v.get("nestweaver"))
                .ok_or_else(|| anyhow::anyhow!("NestWeaver registration missing"))?,
        )
        .map_err(Into::into);
    }
    let target = json_registration(name, base)?
        .ok_or_else(|| anyhow::anyhow!("tool does not have a generated MCP registration"))?;
    let config = user_config::read_json_config(&target.path, SETUP_COMMAND)?;
    let entry = config
        .value
        .get(target.servers_key)
        .and_then(|servers| servers.get("nestweaver"))
        .cloned()
        .context("NestWeaver registration missing")?;
    Ok(normalize_registration(entry))
}

/// Present an OpenCode-shaped entry (`command` is the whole argv,
/// `environment` the env) in the `command`/`args`/`env` shape the server probe
/// reads. Every other entry is returned unchanged.
fn normalize_registration(entry: serde_json::Value) -> serde_json::Value {
    let Some(argv) = entry.get("command").and_then(|c| c.as_array()) else {
        return entry;
    };
    let Some((program, args)) = argv.split_first() else {
        return entry;
    };
    let mut normalized = serde_json::json!({ "command": program, "args": args });
    if let Some(environment) = entry.get("environment") {
        normalized["env"] = environment.clone();
    }
    normalized
}

/// Human hint for why a tool counts as "not detected".
fn detection_hint(name: &str) -> &'static str {
    match name {
        "claude-code" => "no .claude/ directory and no `claude` on PATH",
        "cursor" => "no .cursor/ directory",
        "codex" => "no `codex` on PATH",
        "windsurf" => "no ~/.codeium directory",
        "jetbrains" => "no .idea/ or .junie/ directory",
        "vscode" => "no .vscode/ directory",
        "gemini" => "no .gemini/ directory and no `gemini` on PATH",
        "copilot" => "no .github/copilot-instructions.md and no `gh` on PATH",
        "aider" => "no .aider.conf.yml and no `aider` on PATH",
        "kiro" => "no .kiro/ directory and no `kiro` on PATH",
        "continue" => "no .continue/ directory",
        "cline" => "no .cline/ directory",
        "opencode" => "no opencode.json, no .opencode/ directory and no `opencode` on PATH",
        "trae" => "no .trae/ directory",
        "devin" => "no devin.json and no `devin` on PATH",
        "hermes" => "no .hermes/ directory and no `hermes` on PATH",
        _ => "not recognised",
    }
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
    if let Some(specific) = tool
        && !KNOWN_TOOLS.contains(&specific)
    {
        anyhow::bail!(
            "unknown tool '{specific}'; valid options: {}",
            KNOWN_TOOLS.join(", ")
        );
    }
    let db = absolute_db_path(db_path)?;
    print_setup_banner(&db);
    let ctx = SetupCtx {
        db: &db,
        base,
        force: force_overwrite,
        force_global: force_overwrite && tool.is_some(),
        probe: true,
    };
    run_setup_for_tools(tool, &ctx, force_all, detect_tools(base))
}

/// The body of [`run_setup`] over an explicit tool list, so tests can drive it
/// without probing PATH or writing to the real home directory.
fn run_setup_for_tools(
    tool: Option<&str>,
    ctx: &SetupCtx,
    force_all: bool,
    tools: Vec<ToolSetup>,
) -> Result<(), anyhow::Error> {
    let base = ctx.base;

    // Every selected tool is attempted, whatever happened to the one before
    // it. A failure used to `?` straight out of this loop, so a refusal on
    // Codex's AGENTS.md left every later tool unattempted, and the one error
    // printed ("NestWeaver changed nothing") was about one file while Claude
    // Code's and Codex's other files had already been written.
    let mut attempted = 0usize;
    let mut failures: Vec<&str> = Vec::new();
    for t in tools {
        if let Some(specific) = tool
            && t.name != specific
        {
            continue;
        }

        if !t.detected && !force_all {
            println!(
                "✗ {} — not detected ({})",
                format_name(t.name),
                detection_hint(t.name)
            );
            continue;
        }

        attempted += 1;
        let mut items = Items::new();
        match configure_tool(t.name, ctx, &mut items) {
            Ok(()) => {
                print_result(format_name(t.name), &items);
                if ctx.probe {
                    probe_configured_tool(t.name, base);
                }
            }
            Err(error) => {
                print_failure(format_name(t.name), &items, &error);
                failures.push(t.name);
            }
        }
    }

    if let Some(specific) = tool
        && attempted == 0
    {
        anyhow::bail!(
            "{} was not detected in {} ({}); nothing was written. Pass --all to \
             configure it anyway: nestweaver setup {specific} --all",
            format_name(specific),
            base.display(),
            detection_hint(specific),
        );
    }

    println!();
    if !ctx.db.exists() {
        println!("Run `nestweaver index --repo .` to index this repository.");
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "setup failed for {} (see the errors above); {} other tool(s) were configured",
            failures
                .iter()
                .map(|name| format_name(name))
                .collect::<Vec<_>>()
                .join(", "),
            attempted - failures.len(),
        );
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
            detected: base.join("opencode.json").exists()
                || base.join(".opencode").exists()
                || which_exists("opencode"),
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

/// Build and merge the generated JSON registration for `target`, and record
/// the outcome. Returns the outcome so a caller can bind secondary artifacts
/// (Claude Code's hooks) to the database the registration actually uses.
fn register_json(
    ctx: &SetupCtx,
    items: &mut Items,
    target: &JsonRegistration,
    lite: bool,
    added_status: &str,
) -> Result<Registration, anyhow::Error> {
    let db_str = ctx.db_str();
    let args = if lite {
        mcp_args_lite(&db_str, ctx.base)?
    } else {
        mcp_args(&db_str, ctx.base)?
    };
    let mut entry = serde_json::Map::new();
    if target.servers_key == "mcp" {
        // OpenCode: one `command` array holding the whole argv.
        let mut argv = vec![serde_json::json!("nestweaver")];
        argv.extend(args.as_array().cloned().unwrap_or_default());
        entry.insert("type".to_string(), serde_json::json!("local"));
        entry.insert("command".to_string(), serde_json::Value::Array(argv));
        entry.insert("enabled".to_string(), serde_json::json!(true));
    } else {
        if target.servers_key == "servers" {
            // VS Code's schema names the transport explicitly.
            entry.insert("type".to_string(), serde_json::json!("stdio"));
        }
        entry.insert("command".to_string(), serde_json::json!("nestweaver"));
        entry.insert("args".to_string(), args);
    }
    let outcome = merge_json_mcp(
        &target.path,
        target.servers_key,
        "nestweaver",
        &serde_json::Value::Object(entry),
        ctx.force,
        ctx.base,
    )?;
    items.push((target.label.clone(), outcome.status(added_status)));
    Ok(outcome)
}

fn setup_claude_code(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let base = ctx.base;
    std::fs::create_dir_all(base.join(".claude"))?;
    let target =
        json_registration("claude-code", base)?.context("claude-code has a JSON registration")?;
    let outcome = register_json(ctx, items, &target, false, "MCP server configured")?;

    std::fs::create_dir_all(base.join(".claude/skills/nestweaver"))?;
    let skill_path = base.join(".claude/skills/nestweaver/SKILL.md");
    let skill_status = if skill_path.exists() && !ctx.force {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&skill_path, generate_skill_content())?;
        "skill written"
    };
    items.push((
        ".claude/skills/nestweaver/SKILL.md".to_string(),
        skill_status.to_string(),
    ));

    // The hooks name a database too. Bind them to the one the `.mcp.json`
    // registration actually uses, so a registration left pointing elsewhere
    // (no --force) and the hooks cannot disagree.
    let hooks_db = outcome.effective_db(&ctx.db_str());
    let hooks_status = install_claude_hooks(&hooks_db, base)?;
    items.push((
        ".claude/settings.json".to_string(),
        hooks_status.to_string(),
    ));
    Ok(())
}

/// Quote `value` as ONE POSIX shell word.
///
/// The database path is user input interpolated into hook commands that a
/// shell executes on every session start; unquoted, `--db "a;touch PWNED;b"`
/// was command injection and any path with a space split into several
/// arguments. Single quotes disable every expansion; an embedded `'` is closed,
/// escaped, and reopened.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The SessionStart command setup writes is
/// `{PREFIX}<database as one shell word>{SUFFIX}`. Releases before the quoting
/// fix wrote the same shape with the path unquoted; both are recognised as
/// ours, and nothing else is.
const CLAUDE_SESSION_START_PREFIX: &str = "nestweaver brain status --db ";
const CLAUDE_SESSION_START_SUFFIX: &str =
    " 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'";
/// Opening words of the PreToolUse hint, shared by every release's hook.
const CLAUDE_PRE_TOOL_SIGNATURE: &str = "NestWeaver is indexed";

/// The PreToolUse(Bash) command written by releases before the `tool_input`
/// fix (it read `.command`, which Claude Code never sends). Kept verbatim so a re-run of
/// `setup` can find and replace it; nothing broader is matched.
const LEGACY_CLAUDE_PRE_TOOL_USE_COMMAND: &str = "INPUT=$(cat); CMD=$(echo \"$INPUT\" | jq -r '.command // empty'); \
     if echo \"$CMD\" | grep -qE '(grep|rg|find|fd|ack|ag)\\s'; then \
       echo '{\"additionalContext\": \"NestWeaver is indexed — prefer `brain_search` (searches code + notes) or `brain_context` (ranked structural context) over grep/find. Token savings: ~90% fewer tokens than file-by-file exploration.\"}'; \
     fi";

/// The jq program the PreToolUse(Bash) hook runs on the event: `true` when a
/// search tool is the INVOKED command of some pipeline segment.
///
/// Quoted strings are blanked first, in ONE left-to-right pass so whichever
/// quote opens first wins (an apostrophe inside double quotes must not open a
/// single-quoted run), so `git commit -m "find the bug"` has no `find` left
/// in it; the command is then split at `|`, `;`, `&`, `(`, `)`,
/// backticks and newlines, and each segment's first word — after `VAR=value` assignments
/// and wrappers such as `sudo`, `xargs`, `if` — must be the tool, optionally
/// with a directory (`/usr/bin/grep`). `echo find me` stays silent. `\u0027` is
/// a single quote, which cannot appear inside the shell's single quotes.
const CLAUDE_PRE_TOOL_USE_JQ: &str = r#"(.tool_input.command // "") | gsub("\u0027[^\u0027]*\u0027|\"(\\\\.|[^\"\\\\])*\""; "Q") | [splits("[|;&()`\n]")] | any(.[]; test("^\\s*([A-Za-z_][A-Za-z0-9_]*=\\S*\\s+)*((sudo|command|exec|time|nice|env|xargs|if|then|else|elif|do|while|until|!)\\s+)*(\\S*/)?(grep|egrep|fgrep|rg|find|fd|ack|ag)(\\s|$)"))"#;

fn claude_session_start_command(db_str: &str) -> String {
    // SessionStart is one of the events whose plain stdout Claude Code adds to
    // the session context, so the status text needs no JSON envelope.
    format!(
        "{CLAUDE_SESSION_START_PREFIX}{}{CLAUDE_SESSION_START_SUFFIX}",
        shell_quote(db_str)
    )
}

/// The hook's output: the hint as `hookSpecificOutput.additionalContext`,
/// shell-quoted for `printf`.
fn claude_pre_tool_use_context() -> String {
    let context = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": format!(
                "{CLAUDE_PRE_TOOL_SIGNATURE} — prefer `brain_search` (searches code + notes), \
                 `regex_search`, or `brain_context` (ranked structural context) over grep/find."
            ),
        }
    })
    .to_string();
    shell_quote(&context)
}

/// PreToolUse(Bash): when the agent is about to run a text search, add a hint
/// pointing at the graph tools. Non-blocking: it never sets a permission
/// decision and always exits 0.
///
/// Claude Code sends the tool's arguments under `tool_input`
/// (`.tool_input.command`), and reads a PreToolUse hook's stdout as JSON with
/// the context under `hookSpecificOutput.additionalContext`.
///
/// The search tool must be the command being run ([`CLAUDE_PRE_TOOL_USE_JQ`]),
/// not a word anywhere in it: `echo find me` and `git commit -m "find the
/// bug"` are not searches. Without jq the hook does nothing. Matching the raw
/// event JSON instead fired on the `description` field and on every argument.
fn claude_pre_tool_use_command() -> String {
    format!(
        "command -v jq >/dev/null 2>&1 || exit 0; \
         HIT=$(jq -r {} 2>/dev/null) || exit 0; \
         if [ \"$HIT\" = true ]; then \
           printf '%s\\n' {}; \
         fi; \
         exit 0",
        shell_quote(CLAUDE_PRE_TOOL_USE_JQ),
        claude_pre_tool_use_context()
    )
}

/// The PreToolUse(Bash) command the first `tool_input` release wrote: it
/// matched a search word anywhere in the command, and fell back to the raw
/// event JSON without jq. Recognised so a re-run replaces it.
fn previous_claude_pre_tool_use_command() -> String {
    format!(
        "INPUT=$(cat); \
         CMD=$(printf '%s' \"$INPUT\" | jq -r '.tool_input.command // empty' 2>/dev/null) || CMD=\"$INPUT\"; \
         if printf '%s' \"$CMD\" | grep -qE '(^|[^[:alnum:]_.-])(grep|rg|find|fd|ack|ag)[[:space:]]'; then \
           printf '%s\\n' {}; \
         fi; \
         exit 0",
        claude_pre_tool_use_context()
    )
}

/// True when `word` is exactly one shell word as setup writes a database path:
/// [`shell_quote`]d, or (releases before the quoting fix) bare with no
/// whitespace or shell metacharacter — so `./a.lbug;my-thing` is not ours.
/// An old unquoted install whose path held a space never worked and is not
/// recognised; a re-run adds the working hook beside it.
fn is_one_shell_word(word: &str) -> bool {
    if let Some(quoted) = word.strip_prefix('\'') {
        return quoted
            .strip_suffix('\'')
            .is_some_and(|inner| shell_quote(&inner.replace(r"'\''", "'")) == word);
    }
    !word.is_empty()
        && !word
            .chars()
            .any(|c| c.is_whitespace() || ";|&$()<>'\"\\`*?[]{}".contains(c))
}

/// True when `hook` is one of the entries this module generated — the exact
/// command shapes setup writes now or wrote before, and nothing broader. A
/// substring match replaced a user's own hook that merely ran
/// `nestweaver brain status --db …`.
fn is_nestweaver_hook(hook: &serde_json::Value) -> bool {
    let Some(command) = hook.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    if let Some(db) = command
        .strip_prefix(CLAUDE_SESSION_START_PREFIX)
        .and_then(|rest| rest.strip_suffix(CLAUDE_SESSION_START_SUFFIX))
    {
        return is_one_shell_word(db);
    }
    command == claude_pre_tool_use_command()
        || command == previous_claude_pre_tool_use_command()
        || command == LEGACY_CLAUDE_PRE_TOOL_USE_COMMAND
}

/// Drop NestWeaver's own hooks from one event's matcher list, and any matcher
/// group left empty by that. The user's hooks are untouched.
fn remove_nestweaver_hooks(event: &mut serde_json::Value) {
    let Some(groups) = event.as_array_mut() else {
        return;
    };
    for group in groups.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            hooks.retain(|hook| !is_nestweaver_hook(hook));
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_none_or(|hooks| !hooks.is_empty())
    });
}

/// Install NestWeaver hooks into `.claude/settings.json`.
///
/// Adds a SessionStart hook that prints brain status so the agent knows
/// NestWeaver is available, and a PreToolUse hook for Bash that suggests
/// graph alternatives when the agent falls back to grep/find.
///
/// NestWeaver's own entries are regenerated on every run, so an install made
/// by an older release — whose PreToolUse hook could never fire — is repaired
/// by re-running `setup`.
fn install_claude_hooks(db_str: &str, base: &Path) -> Result<&'static str, anyhow::Error> {
    let settings_path = base.join(".claude/settings.json");
    // `unwrap_or_else(|_| json!({}))` here was silent DATA LOSS. A settings
    // file with a trailing comma, a merge-conflict marker, or a half-saved
    // edit parsed as an error, became an empty object, and was then written
    // back containing only NestWeaver's hooks — destroying the user's
    // permissions, env, model config and every other hook they had. The
    // command then reported "hooks installed".
    //
    // `merge_json_mcp` — the same operation on the same kind of file — already
    // refuses and says why. This is that behaviour, so the two agree.
    //
    // The refusal, the JSONC read, the symlink RESOLUTION and the atomic write
    // are shared with `nestweaver admin install-hook` — which had this defect
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
    let Some(hooks_obj) = hooks.as_object_mut() else {
        anyhow::bail!(
            "{} has a \"hooks\" key that is not an object. Fix it manually or \
             remove that key.",
            settings_path.display()
        );
    };
    let had_ours = ["SessionStart", "PreToolUse"].iter().any(|event| {
        hooks_obj
            .get(*event)
            .and_then(|v| v.as_array())
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|hooks| hooks.iter().any(is_nestweaver_hook))
                })
            })
    });

    for (event, group) in [
        (
            "SessionStart",
            serde_json::json!({
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": claude_session_start_command(db_str),
                }]
            }),
        ),
        (
            "PreToolUse",
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": claude_pre_tool_use_command(),
                }]
            }),
        ),
    ] {
        let slot = hooks_obj
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
        if !slot.is_array() {
            anyhow::bail!(
                "{} has a \"hooks.{event}\" key that is not an array. Fix it \
                 manually or remove that key.",
                settings_path.display()
            );
        }
        remove_nestweaver_hooks(slot);
        if let Some(groups) = slot.as_array_mut() {
            groups.push(group);
        }
    }

    // Idempotency before the write guards: there is nothing to write, so there
    // is nothing to refuse, and a repeat run succeeds on a file this function
    // would decline to rewrite.
    if settings == existing.value {
        return Ok("hooks already installed");
    }
    guard_user_owned_write(&settings_path, &existing)?;

    std::fs::create_dir_all(base.join(".claude"))?;
    let formatted = serde_json::to_string_pretty(&settings)?;
    user_config::replace_file_atomically(&settings_path, &formatted, SETUP_COMMAND)?;

    Ok(if had_ours {
        "hooks updated"
    } else {
        "hooks installed"
    })
}

fn setup_cursor(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let base = ctx.base;
    std::fs::create_dir_all(base.join(".cursor"))?;
    let target = json_registration("cursor", base)?.context("cursor has a JSON registration")?;
    register_json(ctx, items, &target, true, "MCP server (lite: 6 tools)")?;

    std::fs::create_dir_all(base.join(".cursor/rules"))?;
    let rule_path = base.join(".cursor/rules/nestweaver.mdc");
    let rule_status = if rule_path.exists() && !ctx.force {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&rule_path, generate_cursor_rule_content())?;
        "agent rules written"
    };
    items.push((
        ".cursor/rules/nestweaver.mdc".to_string(),
        rule_status.to_string(),
    ));
    Ok(())
}

fn setup_codex(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let base = ctx.base;
    std::fs::create_dir_all(base.join(".codex"))?;
    let db_str = ctx.db_str();
    let config_path = base.join(".codex/config.toml");
    let toml_section = format!(
        "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [{}]\n",
        mcp_arg_vec(&db_str, base, false)?
            .iter()
            .map(|arg| format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let outcome = merge_codex_mcp(&config_path, &toml_section, ctx.force, base)?;
    items.push((
        ".codex/config.toml".to_string(),
        outcome.status("MCP server configured"),
    ));

    let agents_path = base.join("AGENTS.md");
    let wrote =
        crate::guide_section::write_marked_section(&agents_path, &generate_agents_md_content())?;
    let agents_status = match wrote {
        crate::guide_section::GuideSectionWrite::Created => "codebase guide written",
        crate::guide_section::GuideSectionWrite::Updated => "generated section updated",
    };
    items.push(("AGENTS.md".to_string(), agents_status.to_string()));
    Ok(())
}

fn setup_windsurf(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let target =
        json_registration("windsurf", ctx.base)?.context("windsurf has a JSON registration")?;
    let ctx = &SetupCtx {
        db: ctx.db,
        base: ctx.base,
        force: ctx.force && ctx.force_global,
        force_global: ctx.force_global,
        probe: ctx.probe,
    };
    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    register_json(ctx, items, &target, false, "MCP server configured")?;
    // The file is per-user, not per-project, and holds ONE `nestweaver`
    // entry. Say so: running setup in a second project reports that the entry
    // points at the first project's database rather than silently keeping it.
    items.push((
        "note".to_string(),
        "this Windsurf config is global (shared by every project); its single \
         `nestweaver` entry binds one database — re-run with --force in another \
         project to rebind it there"
            .to_string(),
    ));
    Ok(())
}

fn setup_copilot(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let base = ctx.base;
    std::fs::create_dir_all(base.join(".vscode"))?;
    let target = json_registration("copilot", base)?.context("copilot has a JSON registration")?;
    register_json(ctx, items, &target, false, "MCP server configured")?;

    std::fs::create_dir_all(base.join(".github"))?;
    let instructions_path = base.join(".github/copilot-instructions.md");
    let instructions_status = if instructions_path.exists() {
        "already exists (not overwritten)"
    } else {
        std::fs::write(&instructions_path, generate_copilot_instructions())?;
        "instructions written"
    };
    items.push((
        ".github/copilot-instructions.md".to_string(),
        instructions_status.to_string(),
    ));
    items.push((
        "note".to_string(),
        "Copilot in VS Code reads .vscode/mcp.json; the Copilot coding agent \
         takes its MCP configuration from the repository's Copilot settings on \
         GitHub, not from a file"
            .to_string(),
    ));
    Ok(())
}

/// The two lines older releases appended to `.aider.conf.yml`.
const LEGACY_AIDER_COMMENT: &str = "# NestWeaver code intelligence";
const LEGACY_AIDER_KEY: &str = "repo-map: nestweaver mcp";

/// Aider has no MCP client, so there is nothing to register.
///
/// Older releases appended `repo-map: nestweaver mcp --db …` to
/// `.aider.conf.yml`. `repo-map` is not an Aider option (its repo-map keys are
/// `map-tokens`, `map-refresh`, `map-multiplier-no-files`), so that line did
/// nothing at best. This removes exactly the two lines NestWeaver wrote and
/// leaves everything else in the file alone.
fn setup_aider(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let config_path = ctx.base.join(".aider.conf.yml");
    let status = match std::fs::read_to_string(&config_path) {
        Ok(existing) if existing.contains(LEGACY_AIDER_KEY) => {
            let cleaned = remove_legacy_aider_block(&existing);
            user_config::replace_file_atomically(&config_path, &cleaned, SETUP_COMMAND)?;
            "removed the invalid `repo-map:` line an older NestWeaver wrote; Aider has \
             no MCP client, so nothing is registered"
        }
        _ => {
            "nothing written: Aider has no MCP client. Use the `nestweaver` CLI from Aider's shell"
        }
    };
    items.push((".aider.conf.yml".to_string(), status.to_string()));
    Ok(())
}

/// Drop exactly what older releases appended: each `repo-map: nestweaver mcp`
/// line, the `# NestWeaver code intelligence` comment directly above it, and
/// the blank separator line directly above that. Every other line, and every
/// line ending (CRLF included), is kept as it was.
fn remove_legacy_aider_block(existing: &str) -> String {
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let mut dropped = vec![false; lines.len()];
    for (index, line) in lines.iter().enumerate() {
        if !line.trim().starts_with(LEGACY_AIDER_KEY) {
            continue;
        }
        dropped[index] = true;
        if index >= 1 && lines[index - 1].trim() == LEGACY_AIDER_COMMENT {
            dropped[index - 1] = true;
            if index >= 2 && lines[index - 2].trim().is_empty() {
                dropped[index - 2] = true;
            }
        }
    }
    lines
        .iter()
        .zip(dropped)
        .filter(|(_, dropped)| !dropped)
        .map(|(line, _)| *line)
        .collect()
}

/// Cline keeps MCP servers in ONE user-level file
/// (`cline_mcp_settings.json`, inside the editor's extension storage, or
/// `~/.cline/data/settings/` for the CLI) and reads no project-level file.
/// `.cline/settings.json`, which setup used to write, is never read. Rather
/// than edit a global file shared by every project, print the entry to add
/// through Cline's "Configure MCP Servers" panel.
fn setup_cline(ctx: &SetupCtx, items: &mut Items) -> Result<(), anyhow::Error> {
    let snippet = serde_json::json!({
        "mcpServers": {
            "nestweaver": {
                "command": "nestweaver",
                "args": mcp_args(&ctx.db_str(), ctx.base)?,
            }
        }
    });
    items.push((
        "cline_mcp_settings.json".to_string(),
        format!(
            "nothing written: Cline has no project-level MCP file. Open Cline's MCP \
             Servers > Configure > Configure MCP Servers and add:\n{}",
            serde_json::to_string_pretty(&snippet)?
        ),
    ));
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// What a merge did to an existing (or absent) registration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Registration {
    /// No entry existed; the generated one was added.
    Added,
    /// The entry already matched.
    Unchanged,
    /// The entry was kept, with deprecated flags stripped or `--config` added.
    Reconciled,
    /// `--force` rebound an entry that named a different database.
    Rebound { previous_db: String },
    /// The entry names a different database and `--force` was not given; it
    /// was left pointing there.
    PointsElsewhere { existing_db: String },
}

/// How an entry with no `--db` is described.
const NO_DB_ARG: &str = "(none — the server's default)";

impl Registration {
    fn status(&self, added: &str) -> String {
        match self {
            Registration::Added => added.to_string(),
            Registration::Unchanged => "already configured".to_string(),
            Registration::Reconciled => "already configured (flags reconciled)".to_string(),
            Registration::Rebound { previous_db } => {
                format!("re-bound to the requested database (--force; was --db {previous_db})")
            }
            Registration::PointsElsewhere { existing_db } => format!(
                "left unchanged: the existing entry uses --db {existing_db}, not the \
                 requested database; re-run with --force to replace it"
            ),
        }
    }

    /// The database the registration uses after the merge.
    fn effective_db(&self, requested: &str) -> String {
        match self {
            Registration::PointsElsewhere { existing_db } if existing_db != NO_DB_ARG => {
                existing_db.clone()
            }
            _ => requested.to_string(),
        }
    }
}

/// The value following `flag` in an argv.
fn flag_value<'a>(args: impl IntoIterator<Item = Option<&'a str>>, flag: &str) -> Option<String> {
    let args: Vec<Option<&str>> = args.into_iter().collect();
    args.iter()
        .position(|a| *a == Some(flag))
        .and_then(|i| args.get(i + 1).copied().flatten())
        .map(str::to_string)
}

/// Whether two `--db` values name the same database. A relative value is read
/// against `base`, the directory setup configures (project-scoped hosts launch
/// the server from there), so an older `./nestweaver.lbug` entry and the
/// absolute path setup now writes compare equal.
fn same_db(existing: &str, desired: &str, base: &Path) -> bool {
    let resolve = |value: &str| {
        let path = Path::new(value);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };
        joined
            .canonicalize()
            .ok()
            .or_else(|| std::path::absolute(&joined).ok())
            .unwrap_or(joined)
    };
    existing == desired || resolve(existing) == resolve(desired)
}

/// Decide what to do about the `--db` of an existing entry.
enum DbAction {
    Keep,
    Replace { previous: String },
    Elsewhere { existing: String },
}

fn decide_db(existing: Option<&str>, desired: Option<&str>, force: bool, base: &Path) -> DbAction {
    let Some(desired) = desired else {
        return DbAction::Keep;
    };
    match existing {
        Some(existing) if existing == desired => DbAction::Keep,
        Some(existing) if same_db(existing, desired, base) && !force => DbAction::Keep,
        Some(existing) if force => DbAction::Replace {
            previous: existing.to_string(),
        },
        Some(existing) => DbAction::Elsewhere {
            existing: existing.to_string(),
        },
        None if force => DbAction::Replace {
            previous: NO_DB_ARG.to_string(),
        },
        None => DbAction::Elsewhere {
            existing: NO_DB_ARG.to_string(),
        },
    }
}

/// Merge an MCP server entry into a JSON config file under `servers_key`
/// (`mcpServers`, or `servers` for VS Code).
///
/// An existing entry is reconciled, never silently left stale: deprecated
/// flags are stripped and a missing `--config` added. When it names a
/// DIFFERENT database, `force` rebinds its `--db` (and `--config`) in place —
/// other user-owned flags such as `--lite` or `--tools` survive — and without
/// `force` the outcome says the entry points elsewhere. Before this, a re-run
/// with another `--db`, even with `--force`, reported "already configured" and
/// kept the old database.
fn merge_json_mcp(
    path: &Path,
    servers_key: &str,
    server_name: &str,
    config: &serde_json::Value,
    force: bool,
    base: &Path,
) -> Result<Registration, anyhow::Error> {
    // The canonical argv is already in `config`; read the desired `--db` and
    // `--config` out of it rather than threading them through every caller.
    let desired_args: Vec<Option<&str>> = entry_argv(config)
        .map(|args| args.iter().map(|a| a.as_str()).collect())
        .unwrap_or_default();
    let desired_db = flag_value(desired_args.iter().copied(), "--db");
    let desired_config = flag_value(desired_args.iter().copied(), "--config");
    // `path.exists()` FOLLOWS SYMLINKS, so a DANGLING `.mcp.json` link read as
    // "absent", sending this straight to the create branch, which then created
    // the link's target outside the project. `read_json_config` judges
    // existence with `symlink_metadata`, and resolves a live link to the file
    // it names so a dotfiles checkout is merged into rather than clobbered.
    let existing = user_config::read_json_config(path, SETUP_COMMAND)?;
    let mut root = existing.value.clone();

    {
        let servers = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("config at {} is not a JSON object", path.display()))?
            .entry(servers_key)
            .or_insert(serde_json::json!({}));

        if servers.get(server_name).is_none() {
            servers
                .as_object_mut()
                .ok_or_else(|| {
                    anyhow::anyhow!("{servers_key} is not an object in {}", path.display())
                })?
                .insert(server_name.to_string(), config.clone());

            guard_user_owned_write(path, &existing)?;
            let json = serde_json::to_string_pretty(&root)?;
            user_config::replace_file_atomically(path, &json, SETUP_COMMAND)?;
            return Ok(Registration::Added);
        }
    }

    // Server already exists.
    //
    // Reconcile it rather than leaving it alone. This branch used to only PRUNE
    // deprecated flags, so a registration written before `--config` existed kept
    // its old argv forever: every upgrade path silently stayed config-less, and
    // re-running `setup` — the obvious remedy — changed nothing. Additive unless
    // `force`, so a hand-tuned `--lite`, `--tools` or a deliberately different
    // `--db` is preserved.
    let mut stripped: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut outcome = Registration::Unchanged;
    {
        let root_obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("config at {} is not a JSON object", path.display()))?;
        if let Some(server_entry) = root_obj
            .get_mut(servers_key)
            .and_then(|s| s.as_object_mut())
            .and_then(|s| s.get_mut(server_name))
            && let Some(args) = entry_argv_mut(server_entry)
        {
            args.retain(|arg| {
                let arg_str = arg.as_str().unwrap_or("");
                let is_deprecated = DEPRECATED_MCP_ARGS.contains(&arg_str);
                if is_deprecated {
                    stripped.push(arg_str.to_string());
                }
                !is_deprecated
            });

            let existing_db = flag_value(args.iter().map(|a| a.as_str()), "--db");
            match decide_db(existing_db.as_deref(), desired_db.as_deref(), force, base) {
                DbAction::Keep => {}
                DbAction::Elsewhere { existing } => {
                    outcome = Registration::PointsElsewhere {
                        existing_db: existing,
                    };
                }
                DbAction::Replace { previous } => {
                    let desired = desired_db.clone().unwrap_or_default();
                    set_json_flag(args, "--db", &desired);
                    outcome = Registration::Rebound {
                        previous_db: previous,
                    };
                }
            }

            let existing_config = flag_value(args.iter().map(|a| a.as_str()), "--config");
            // An entry left pointing at ANOTHER database keeps its own
            // config too: this project's `--config` beside that database
            // would pair two installations, under a status that says "left
            // unchanged".
            let points_elsewhere = matches!(outcome, Registration::PointsElsewhere { .. });
            if !points_elsewhere && let Some(config) = desired_config.as_deref() {
                match existing_config {
                    None => {
                        args.push(serde_json::Value::String("--config".to_string()));
                        args.push(serde_json::Value::String(config.to_string()));
                        added.push("--config".to_string());
                    }
                    Some(current) if force && current != config => {
                        set_json_flag(args, "--config", config);
                        if outcome == Registration::Unchanged {
                            outcome = Registration::Reconciled;
                        }
                    }
                    Some(_) => {}
                }
            }
        }
    }
    if outcome == Registration::Unchanged && (!stripped.is_empty() || !added.is_empty()) {
        outcome = Registration::Reconciled;
    }

    if root != existing.value {
        guard_user_owned_write(path, &existing)?;
        let json = serde_json::to_string_pretty(&root)?;
        user_config::replace_file_atomically(path, &json, SETUP_COMMAND)?;
        for flag in &stripped {
            eprintln!("  (stripped deprecated flag: {flag})");
        }
        for flag in &added {
            eprintln!("  (added missing flag: {flag})");
        }
    }

    Ok(outcome)
}

/// The argv array of a JSON registration: `args`, or — for OpenCode, whose
/// entries carry the whole argv in `command` — the `command` array. A flag
/// search over the full argv is unaffected by the leading program name.
fn entry_argv(entry: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    entry
        .get("args")
        .and_then(|a| a.as_array())
        .or_else(|| entry.get("command").and_then(|c| c.as_array()))
}

fn entry_argv_mut(entry: &mut serde_json::Value) -> Option<&mut Vec<serde_json::Value>> {
    let key = if entry.get("args").is_some_and(|a| a.is_array()) {
        "args"
    } else {
        "command"
    };
    entry.get_mut(key).and_then(|a| a.as_array_mut())
}

/// Set the value after `flag` in a JSON argv, appending the pair if absent.
fn set_json_flag(args: &mut Vec<serde_json::Value>, flag: &str, value: &str) {
    match args.iter().position(|a| a.as_str() == Some(flag)) {
        Some(i) if i + 1 < args.len() => args[i + 1] = serde_json::Value::String(value.into()),
        Some(_) => args.push(serde_json::Value::String(value.into())),
        None => {
            args.push(serde_json::Value::String(flag.into()));
            args.push(serde_json::Value::String(value.into()));
        }
    }
}

/// Set the value after `flag` in a TOML argv, appending the pair if absent.
/// Replaces the one value in place, so comments and layout around it survive.
fn set_toml_flag(args: &mut toml_edit::Array, flag: &str, value: &str) {
    // Bound first: `Array::iter` returns a boxed iterator, which has drop glue,
    // so as a `match` scrutinee its borrow would outlive the arms that mutate.
    let position = args.iter().position(|a| a.as_str() == Some(flag));
    match position {
        Some(i) if i + 1 < args.len() => {
            let decor = args
                .get(i + 1)
                .map(|old| old.decor().clone())
                .unwrap_or_default();
            let mut new_value = toml_edit::Value::from(value);
            *new_value.decor_mut() = decor;
            args.replace(i + 1, new_value);
        }
        Some(_) => args.push(value),
        None => {
            args.push(flag);
            args.push(value);
        }
    }
}

/// Merge the generated Codex MCP registration without replacing user-owned
/// arguments. Same reconciliation rules as [`merge_json_mcp`].
fn merge_codex_mcp(
    path: &Path,
    content: &str,
    force: bool,
    base: &Path,
) -> Result<Registration, anyhow::Error> {
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
    let desired_config = flag_value(desired_args.iter().map(|v| v.as_str()), "--config");
    let desired_db = flag_value(desired_args.iter().map(|v| v.as_str()), "--db");

    // Codex's config is TOML and goes through `toml_edit`, which preserves
    // comments and formatting — so this file has no JSONC problem. It shares
    // the other two: `exists()` followed symlinks, and each `fs::write` below
    // truncated the user's `~/.codex/config.toml` in place.
    let probe = user_config::probe(path)?;
    if !probe.existed {
        user_config::replace_file_atomically(path, content, SETUP_COMMAND)?;
        return Ok(Registration::Added);
    }
    // A symlinked `~/.codex/config.toml` is a dotfiles checkout. Resolve it
    // HERE as well as in the writer, so the section this function decides to
    // append is decided from the same bytes the write will land on, and so a
    // dangling link is reported as a dangling link rather than as a `NotFound`
    // on a path that plainly exists.
    let resolved = user_config::resolve_for_write(path, SETUP_COMMAND)?;

    let existing = std::fs::read_to_string(&resolved)?;
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
        user_config::replace_file_atomically(path, &appended, SETUP_COMMAND)?;
        return Ok(Registration::Added);
    }

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

    let mut outcome = Registration::Unchanged;
    let existing_db = flag_value(args.iter().map(|v| v.as_str()), "--db");
    match decide_db(existing_db.as_deref(), desired_db.as_deref(), force, base) {
        DbAction::Keep => {}
        DbAction::Elsewhere { existing } => {
            outcome = Registration::PointsElsewhere {
                existing_db: existing,
            };
        }
        DbAction::Replace { previous } => {
            set_toml_flag(args, "--db", desired_db.as_deref().unwrap_or_default());
            outcome = Registration::Rebound {
                previous_db: previous,
            };
        }
    }

    let mut added_config = false;
    // As in `merge_json_mcp`: an entry left on another database keeps its
    // own config.
    let points_elsewhere = matches!(outcome, Registration::PointsElsewhere { .. });
    if !points_elsewhere && let Some(desired_config) = desired_config.as_deref() {
        // Bound first, for the same boxed-iterator reason as `set_toml_flag`.
        let config_position = args
            .iter()
            .position(|value| value.as_str() == Some("--config"));
        if let Some(config_index) = config_position {
            let Some(current) = args
                .get(config_index + 1)
                .and_then(toml_edit::Value::as_str)
                .map(str::to_string)
            else {
                anyhow::bail!(
                    "[mcp_servers.nestweaver] in {} has --config without a path",
                    path.display()
                );
            };
            if force && current != desired_config {
                set_toml_flag(args, "--config", desired_config);
                if outcome == Registration::Unchanged {
                    outcome = Registration::Reconciled;
                }
            }
        } else {
            // Add only the missing pair. `--tools`, `--lite`, a custom `--db`,
            // comments, and multiline formatting remain owned by the user.
            args.push("--config");
            args.push(desired_config);
            added_config = true;
            if outcome == Registration::Unchanged {
                outcome = Registration::Reconciled;
            }
        }
    }

    let rendered = document.to_string();
    if rendered != existing {
        user_config::replace_file_atomically(path, &rendered, SETUP_COMMAND)?;
        if added_config {
            eprintln!("  (added missing flag: --config)");
        }
    }
    Ok(outcome)
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
    let mut text = String::from(
        "---\ndescription: Use NestWeaver for structural codebase understanding\nglobs:\nalwaysApply: true\n---\n\n# NestWeaver capability profile v1: lite\n\nConfirm the host exposes these tools before following this guide. Configuration and a server handshake do not prove host activation; restart the host session if the tools are missing.\n\n",
    );
    for (name, description, _, _) in nestweaver_mcp::tools::tool_doc_entries() {
        if nestweaver_mcp::tools::LITE_TOOLS.contains(&name.as_str()) {
            text.push_str(&format!("- `{name}` — {description}\n"));
        }
    }
    text.push_str("\nUse brain_context for structural exploration, brain_impact before changing a symbol, and detect_changes for the changed-file review. This lite registration does not expose every NestWeaver tool. For additional capabilities use the documented CLI command with the same --config and --db bindings, or explicitly enable the full MCP profile. If a capability cannot be verified, report that review step as unverified; do not equate a text search with graph impact analysis.\n");
    text
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

fn print_result(tool_name: &str, items: &Items) {
    println!("✓ {tool_name}");
    for (path, desc) in items {
        println!("  • {path} — {desc}");
    }
}

/// Report a tool that failed part-way: what was already written, then the
/// error. The error's own "NestWeaver changed nothing" is about the one file
/// it names, so the rows above it say what DID change.
fn print_failure(tool_name: &str, items: &Items, error: &anyhow::Error) {
    println!("✗ {tool_name} — failed");
    for (path, desc) in items {
        println!("  • {path} — {desc}");
    }
    eprintln!("  error: {error:#}");
    if !items.is_empty() {
        eprintln!(
            "  note: the error concerns only the file it names; the {} entr{} listed above {} already written.",
            items.len(),
            if items.len() == 1 { "y" } else { "ies" },
            if items.len() == 1 { "was" } else { "were" },
        );
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

    let db = absolute_db_path(db_path)?;
    // Banner once per invocation, and suppressed under --quiet (nw-051).
    if !quiet {
        print_setup_banner(&db);
    }

    let ctx = SetupCtx {
        db: &db,
        base,
        force: false,
        force_global: false,
        probe: true,
    };
    let mut configured = Vec::new();
    let mut failures = Vec::new();
    for &name in to_configure {
        let mut items = Items::new();
        match configure_tool(name, &ctx, &mut items) {
            Ok(()) => {
                if !quiet {
                    print_result(format_name(name), &items);
                }
                configured.push(name);
            }
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
fn test_ctx<'a>(db: &'a Path, base: &'a Path) -> SetupCtx<'a> {
    SetupCtx {
        db,
        base,
        force: false,
        force_global: false,
        probe: false,
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
    fn setup_codex_creates_agents_md_inside_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        let db = tmp.path().join("t.lbug");

        setup_codex(&test_ctx(&db, &base), &mut Items::new()).unwrap();

        let text = std::fs::read_to_string(base.join("AGENTS.md")).unwrap();
        assert!(text.starts_with("<!-- nestweaver:begin -->\n"), "{text}");
        assert!(text.contains("# AGENTS.md — Codebase Intelligence Guide"));
        assert!(
            text.trim_end().ends_with("<!-- nestweaver:end -->"),
            "{text}"
        );
    }

    #[test]
    fn setup_codex_updates_only_the_marked_section() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        let agents = base.join("AGENTS.md");
        std::fs::write(
            &agents,
            "KEEP-ABOVE\n<!-- nestweaver:begin -->\nOLD\n<!-- nestweaver:end -->\nKEEP-BELOW\n",
        )
        .unwrap();
        let db = tmp.path().join("t.lbug");

        setup_codex(&test_ctx(&db, &base), &mut Items::new()).unwrap();

        let text = std::fs::read_to_string(&agents).unwrap();
        assert!(text.starts_with("KEEP-ABOVE\n"), "{text}");
        assert!(text.contains("\nKEEP-BELOW\n"), "{text}");
        assert!(!text.contains("OLD"));
        assert!(text.contains("# AGENTS.md — Codebase Intelligence Guide"));
    }

    #[test]
    fn setup_codex_refuses_an_unmarked_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        let agents = base.join("AGENTS.md");
        let original = "# Hand written\n";
        std::fs::write(&agents, original).unwrap();
        let db = tmp.path().join("t.lbug");

        let error = setup_codex(&test_ctx(&db, &base), &mut Items::new()).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("no NestWeaver section markers"),
            "{message}"
        );
        assert!(message.contains("left unchanged"), "{message}");
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), original);
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

        setup_cursor(&test_ctx(&db, &base), &mut Items::new()).unwrap_err();
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
        merge_json_mcp(
            &path,
            "mcpServers",
            "nestweaver",
            &desired,
            false,
            dir.path(),
        )
        .unwrap();

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

        // The SAME database the section already binds: a section bound to a
        // different one is left entirely alone, --config included (see
        // `a_different_db_is_reported_without_force_and_rebound_with_it`).
        let desired = "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"/custom-db\", \"--config\", \"/cfg/instance.toml\"]\n";
        merge_codex_mcp(&path, desired, false, dir.path()).unwrap();

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
    #[cfg(unix)]
    fn automatic_setup_never_executes_a_preserved_custom_server() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("exists.lbug");
        std::fs::write(&db, "probe precondition only").unwrap();
        std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
        let marker = dir.path().join("must-not-run");
        let entry = serde_json::json!({"command":"sh", "args":["-c",r#"touch "$1""#,"sh",marker,"--db",db]});
        std::fs::write(
            dir.path().join(".cursor/mcp.json"),
            serde_json::json!({"mcpServers":{"nestweaver":entry.clone()}}).to_string(),
        )
        .unwrap();
        run_auto_setup_for_tools(&db, dir.path(), true, &["cursor"]).unwrap();
        assert!(
            !marker.exists(),
            "ordinary indexing must not execute repository MCP configuration"
        );
        assert_eq!(configured_mcp_entry("cursor", dir.path()).unwrap(), entry);
    }

    #[test]
    fn cursor_guide_uses_only_the_registered_lite_tool_catalogue() {
        let guide = generate_cursor_rule_content();
        for name in nestweaver_mcp::tools::LITE_TOOLS {
            assert!(guide.contains(&format!("`{name}`")));
        }
        for unavailable in [
            "read_symbols",
            "investigate_hydrate",
            "dead_code",
            "project_context",
        ] {
            assert!(
                !guide.contains(&format!("- `{unavailable}`")),
                "lite guide advertises unavailable tool {unavailable}"
            );
        }
        assert!(guide.contains("unverified"));
    }

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

/// nw-QUICK-1: a symlinked user config is RESOLVED, not refused.
///
/// `abbde5d8` made every `setup` writer refuse a symbolic link. That was the
/// right call for `server init-tls`, which was writing a CA private key
/// through a dangling link and landing it outside `--output-dir`, and the
/// wrong call here: symlinking `.mcp.json`, `.claude/settings.json` or
/// `~/.codex/config.toml` into a dotfiles repository is a normal, deliberate
/// workflow that the refusal broke.
///
/// Resolving is not "delete the check". `atomic_replace_file` renames a temp
/// file over its destination, so applied to the LINK it replaces the link with
/// a regular file — the target keeps its old contents and the dotfiles link is
/// gone. Every test below therefore asserts BOTH halves: the target changed,
/// and the link is still a link.
#[cfg(all(test, unix))]
mod symlinked_config_tests {
    use super::*;

    /// Build `project/` and `dotfiles/`, put `name` in the dotfiles repo with
    /// `content`, and link `project/<link_name>` at it. Returns (project,
    /// target, link).
    fn dotfiles_link(
        dir: &std::path::Path,
        link_name: &str,
        target_name: &str,
        content: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let project = dir.join("project");
        let dotfiles = dir.join("dotfiles");
        let link = project.join(link_name);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join(target_name);
        std::fs::write(&target, content).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        (project, target, link)
    }

    fn assert_still_a_link(link: &std::path::Path, target: &std::path::Path) {
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{} stopped being a symbolic link — the dotfiles checkout it \
             belonged to is now broken",
            link.display()
        );
        assert_eq!(std::fs::read_link(link).unwrap(), target);
    }

    #[test]
    fn a_symlinked_mcp_json_is_merged_into_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let (_project, target, link) = dotfiles_link(
            dir.path(),
            ".mcp.json",
            "mcp.json",
            r#"{"mcpServers":{"other":{"command":"other"}}}"#,
        );

        let added = merge_json_mcp(
            &link,
            "mcpServers",
            "nestweaver",
            &serde_json::json!({"command": "nestweaver", "args": ["mcp"]}),
            false,
            dir.path(),
        )
        .expect("a symlinked .mcp.json is a dotfiles checkout, not an error");
        assert_eq!(added, Registration::Added);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["nestweaver"]["command"], "nestweaver");
        assert_eq!(
            written["mcpServers"]["other"]["command"], "other",
            "the user's other server must survive: {written}"
        );
        assert_still_a_link(&link, &target);
    }

    #[test]
    fn a_symlinked_claude_settings_gains_the_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let (project, target, link) = dotfiles_link(
            dir.path(),
            ".claude/settings.json",
            "settings.json",
            r#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"]}}"#,
        );

        let status = install_claude_hooks("/tmp/x.lbug", &project)
            .expect("a symlinked settings.json is a dotfiles checkout, not an error");
        assert_eq!(status, "hooks installed");

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(written["model"], "opus", "{written}");
        assert!(written["hooks"]["SessionStart"].is_array(), "{written}");
        assert_still_a_link(&link, &target);
    }

    #[test]
    fn a_symlinked_codex_config_gains_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let (_project, target, link) = dotfiles_link(
            dir.path(),
            "config.toml",
            "codex-config.toml",
            "[history]\npersistence = \"none\"\n",
        );

        let desired = "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"/db\", \"--config\", \"/cfg/instance.toml\"]\n";
        assert_eq!(
            merge_codex_mcp(&link, desired, false, dir.path())
                .expect("a symlinked config.toml is a dotfiles checkout, not an error"),
            Registration::Added
        );

        let written = std::fs::read_to_string(&target).unwrap();
        assert!(
            written.contains("[mcp_servers.nestweaver]"),
            "the section must land in the target: {written}"
        );
        assert!(
            written.contains("persistence = \"none\""),
            "the user's own settings must survive: {written}"
        );
        assert_still_a_link(&link, &target);
    }

    /// A link to a file that is not there. There is nothing to merge into, and
    /// creating the target would write a config to a path `setup` was never
    /// pointed at — the `server init-tls` hijack, in a different file.
    #[test]
    fn a_dangling_symlinked_config_is_refused_by_every_writer() {
        for name in [".mcp.json", "config.toml"] {
            let dir = tempfile::tempdir().unwrap();
            let link = dir.path().join(name);
            let missing = dir.path().join("elsewhere/gone");
            std::os::unix::fs::symlink(&missing, &link).unwrap();

            let error = if name == "config.toml" {
                merge_codex_mcp(
                    &link,
                    "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\"]\n",
                    false,
                    dir.path(),
                )
                .unwrap_err()
            } else {
                merge_json_mcp(
                    &link,
                    "mcpServers",
                    "nestweaver",
                    &serde_json::json!({"command": "nestweaver", "args": ["mcp"]}),
                    false,
                    dir.path(),
                )
                .unwrap_err()
            };

            let message = format!("{error:#}");
            assert!(message.contains("gone"), "for {name}: {message}");
            assert!(message.contains("changed nothing"), "for {name}: {message}");
            assert!(
                !missing.exists(),
                "for {name}: nothing may be created there"
            );
        }
    }
}

/// QA round 2026-10 (integrations): the generated Claude Code hooks, the
/// per-tool failure isolation, `--db` rebinding, and the host formats.
#[cfg(test)]
mod integration_fix_tests {
    use super::*;

    fn settings(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
            .unwrap()
    }

    fn hook_command(settings: &serde_json::Value, event: &str) -> String {
        settings["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap().to_string())
            .find(|c| c.contains("nestweaver") || c.contains("NestWeaver"))
            .unwrap_or_else(|| panic!("no NestWeaver {event} hook in {settings}"))
    }

    #[cfg(unix)]
    fn run_sh(command: &str, stdin: &str, path_prefix: Option<&Path>) -> std::process::Output {
        use std::io::Write as _;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(prefix) = path_prefix {
            let path = format!(
                "{}:{}",
                prefix.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            cmd.env("PATH", path);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// The PreToolUse hook read `.command` (Claude Code sends
    /// `.tool_input.command`) and printed a top-level `additionalContext`
    /// (Claude Code reads `hookSpecificOutput.additionalContext`). Run the
    /// generated command on a real PreToolUse payload.
    #[test]
    #[cfg(unix)]
    fn pre_tool_use_hook_reads_tool_input_and_emits_hook_specific_output() {
        if !jq_available() {
            eprintln!("skipping: jq not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        install_claude_hooks("/tmp/x.lbug", dir.path()).unwrap();
        let command = hook_command(&settings(dir.path()), "PreToolUse");

        let event = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "rg -n handleLogin src/", "description": "search"}
        })
        .to_string();
        let out = run_sh(&command, &event, None);
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("hook stdout must be JSON ({e}): {stdout:?}"));
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(
            parsed["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("brain_search"),
            "{parsed}"
        );
        assert!(parsed.get("additionalContext").is_none(), "{parsed}");

        // A command that is not a search stays silent.
        let quiet = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo build"}
        })
        .to_string();
        let out = run_sh(&command, &quiet, None);
        assert!(out.status.success());
        assert!(
            out.stdout.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// `--db "$Q/B/d;touch PWNED;x/db.lbug"` was interpolated unquoted into
    /// the SessionStart command — command injection, and any space split the
    /// path. The path must reach `nestweaver` as ONE argument, verbatim.
    #[test]
    #[cfg(unix)]
    fn session_start_hook_quotes_the_database_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pwned = dir.path().join("PWNED");
        let hostile = format!(
            "{}/my dir/it's;touch {};x/$(touch {}).lbug",
            dir.path().display(),
            pwned.display(),
            pwned.display()
        );
        install_claude_hooks(&hostile, dir.path()).unwrap();
        let command = hook_command(&settings(dir.path()), "SessionStart");

        // A stub `nestweaver` that records its argv one per line.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let record = dir.path().join("argv");
        let stub = bin.join("nestweaver");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = run_sh(&command, "{}", Some(&bin));
        assert!(out.status.success(), "{out:?}");
        assert!(!pwned.exists(), "the database path was executed as shell");
        let argv = std::fs::read_to_string(&record).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(argv, ["brain", "status", "--db", hostile.as_str()]);
    }

    /// An install made by an older release (the broken `.command` hook) is
    /// repaired by re-running setup, the user's own hooks survive, and a third
    /// run is a no-op.
    #[test]
    fn rerun_replaces_legacy_hooks_and_keeps_the_users() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        // The exact commands older releases wrote: an unquoted database path,
        // and the PreToolUse hook that read `.command`.
        let legacy_session_start =
            format!("{CLAUDE_SESSION_START_PREFIX}./nestweaver.lbug{CLAUDE_SESSION_START_SUFFIX}");
        let legacy = serde_json::json!({
            "hooks": {
                "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": legacy_session_start}]}],
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": LEGACY_CLAUDE_PRE_TOOL_USE_COMMAND}]},
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "my-own-hook.sh"}]}
                ]
            }
        });
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        assert_eq!(
            install_claude_hooks("/abs/db.lbug", dir.path()).unwrap(),
            "hooks updated"
        );
        let after = settings(dir.path());
        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "{after}");
        assert!(
            pre.iter()
                .any(|g| g["hooks"][0]["command"] == "my-own-hook.sh")
        );
        let ours = hook_command(&after, "PreToolUse");
        assert!(ours.contains(".tool_input.command"), "{ours}");
        assert!(!ours.contains("'.command"), "{ours}");
        assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(hook_command(&after, "SessionStart").contains("'/abs/db.lbug'"));

        assert_eq!(
            install_claude_hooks("/abs/db.lbug", dir.path()).unwrap(),
            "hooks already installed"
        );
    }

    /// Only the exact command shapes setup writes are ours. A substring match
    /// on `nestweaver brain status --db` replaced the user's own hook.
    #[test]
    fn only_the_commands_setup_writes_are_recognised_as_ours() {
        let hook = |command: &str| serde_json::json!({"type": "command", "command": command});
        for ours in [
            claude_session_start_command("/abs/db.lbug"),
            claude_session_start_command("/my dir/it's.lbug"),
            format!("{CLAUDE_SESSION_START_PREFIX}./nestweaver.lbug{CLAUDE_SESSION_START_SUFFIX}"),
            claude_pre_tool_use_command(),
            previous_claude_pre_tool_use_command(),
            LEGACY_CLAUDE_PRE_TOOL_USE_COMMAND.to_string(),
        ] {
            assert!(is_nestweaver_hook(&hook(ours.as_str())), "{ours}");
        }
        for users in [
            "nestweaver brain status --db ./mine.lbug",
            "nestweaver brain status --db ./mine.lbug --json | my-formatter",
            "nestweaver brain status --db ./a.lbug; my-thing 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'",
            "nestweaver brain status --db ./a.lbug;my-thing 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'",
            "nestweaver brain status --db $(pick-db) 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'",
            "nestweaver brain status --db 'a' 'b' 2>/dev/null || echo 'NestWeaver: not indexed yet (run: nestweaver index --repo .)'",
            "echo 'NestWeaver is indexed' && my-check",
            "my-own-hook.sh",
        ] {
            assert!(!is_nestweaver_hook(&hook(users)), "{users}");
        }
        assert!(!is_nestweaver_hook(&serde_json::json!({"type": "command"})));
    }

    /// A user's own SessionStart hook that runs `nestweaver brain status` and a
    /// PreToolUse hook that mentions the hint text survive a re-run, and the
    /// first `tool_input` release's PreToolUse hook is replaced.
    #[test]
    fn a_users_lookalike_hooks_survive_and_the_previous_release_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let mine = "nestweaver brain status --db ./mine.lbug --json | my-formatter";
        let my_check = "echo 'NestWeaver is indexed' && my-check";
        let previous = previous_claude_pre_tool_use_command();
        let before = serde_json::json!({
            "hooks": {
                "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": mine}]}],
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": previous}]},
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": my_check}]}
                ]
            }
        });
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&before).unwrap(),
        )
        .unwrap();

        assert_eq!(
            install_claude_hooks("/abs/db.lbug", dir.path()).unwrap(),
            "hooks updated"
        );
        let after = settings(dir.path());
        let commands = |event: &str| -> Vec<String> {
            after["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                .map(|h| h["command"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            commands("SessionStart"),
            [
                mine.to_string(),
                claude_session_start_command("/abs/db.lbug")
            ],
            "{after}"
        );
        assert_eq!(
            commands("PreToolUse"),
            [my_check.to_string(), claude_pre_tool_use_command()],
            "{after}"
        );
    }

    #[cfg(unix)]
    fn jq_available() -> bool {
        which_exists("jq")
    }

    /// The hook fired on `echo find me` and `git commit -m "find the bug"`: a
    /// search word anywhere counted. Only a search tool that is the INVOKED
    /// command of a pipeline segment does.
    #[test]
    #[cfg(unix)]
    fn pre_tool_use_hook_fires_only_when_a_search_tool_is_the_command() {
        if !jq_available() {
            eprintln!("skipping: jq not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        install_claude_hooks("/tmp/x.lbug", dir.path()).unwrap();
        let command = hook_command(&settings(dir.path()), "PreToolUse");
        let fires = |bash: &str| {
            let event = serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                // The description names a search: it must not count.
                "tool_input": {"command": bash, "description": "grep and find things"}
            })
            .to_string();
            let out = run_sh(&command, &event, None);
            assert!(out.status.success(), "{bash}: {out:?}");
            !out.stdout.is_empty()
        };
        for search in [
            "rg -n handleLogin src/",
            "grep -rn \"foo bar\" src",
            "cat x | grep foo",
            "cd src && find . -name '*.rs'",
            "FOO=1 /usr/bin/grep x y",
            "ls; rg x",
            "echo $(find . -type f)",
            "ls | xargs grep foo",
            // An apostrophe inside double quotes opens no single-quoted run.
            "echo \"it's\"; grep x; echo 'y'",
        ] {
            assert!(fires(search), "must fire on {search:?}");
        }
        for other in [
            "echo find me",
            "git commit -m \"find the bug\"",
            "git commit -m 'grep it; rg x'",
            "git log --grep foo",
            "echo grep",
            "./find-dupes.sh",
            "cargo build",
        ] {
            assert!(!fires(other), "must stay silent on {other:?}");
        }
    }

    /// Without jq the hook matched the raw event JSON — the `description`
    /// field, every argument. It now does nothing rather than guess.
    #[test]
    #[cfg(unix)]
    fn pre_tool_use_hook_without_jq_does_nothing() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        install_claude_hooks("/tmp/x.lbug", dir.path()).unwrap();
        let command = hook_command(&settings(dir.path()), "PreToolUse");
        let empty_path = dir.path().join("no-tools");
        std::fs::create_dir_all(&empty_path).unwrap();
        let event = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "rg -n handleLogin src/", "description": "grep"}
        })
        .to_string();
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .env("PATH", &empty_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // With no jq the hook exits before reading stdin. Either the small
        // event is queued first or the successful early exit closes the pipe.
        let input_result = child.stdin.take().unwrap().write_all(event.as_bytes());
        let out = child.wait_with_output().unwrap();
        if let Err(error) = input_result {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
        }
        assert!(out.status.success(), "{out:?}");
        assert!(
            out.stdout.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// The shipped `integrations/claude-code/hooks` scripts, run with a stub
    /// `nestweaver` that records the argv of its last call.
    #[cfg(unix)]
    struct ShippedHook {
        dir: tempfile::TempDir,
        record: std::path::PathBuf,
        bin: std::path::PathBuf,
        db: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl ShippedHook {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let record = dir.path().join("argv");
            let stub = bin.join("nestweaver");
            std::fs::write(
                &stub,
                format!(
                    "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\nprintf '{{\"results\": []}}'\n",
                    record.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            let db = dir.path().join("graph.lbug");
            std::fs::write(&db, "").unwrap();
            Self {
                dir,
                record,
                bin,
                db,
            }
        }

        /// Run `script` on `event`; the argv of the stub's last call, if any.
        fn run(&self, script: &str, event: &serde_json::Value) -> Option<Vec<String>> {
            use std::io::Write as _;
            let _ = std::fs::remove_file(&self.record);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("integrations/claude-code/hooks")
                .join(script);
            let mut child = std::process::Command::new("bash")
                .arg(&path)
                .current_dir(self.dir.path())
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.bin.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("NESTWEAVER_DB", &self.db)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(event.to_string().as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{script}: {out:?}");
            std::fs::read_to_string(&self.record)
                .ok()
                .map(|argv| argv.lines().map(str::to_string).collect())
        }
    }

    /// `pre-search-enrich.sh` searched the LAST token — the path:
    /// `grep -rn "foo bar" src` became `search -- src`. It must search the
    /// pattern, and search nothing when the pattern is not on the command line.
    #[test]
    #[cfg(unix)]
    fn shipped_search_hook_searches_the_pattern_not_the_path() {
        if !jq_available() {
            eprintln!("skipping: jq not on PATH");
            return;
        }
        let hook = ShippedHook::new();
        let searched = |command: &str| {
            let event = serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": command, "description": "find src"}
            });
            hook.run("pre-search-enrich.sh", &event).map(|argv| {
                assert_eq!(argv[..2], ["search", "--json"], "{command}: {argv:?}");
                assert_eq!(argv[argv.len() - 2], "--", "{command}: {argv:?}");
                argv.last().unwrap().clone()
            })
        };
        for (command, pattern) in [
            ("grep -rn \"foo bar\" src", "foo bar"),
            ("grep -rn 'it''s' src", "its"),
            ("rg -e handleLogin src/", "handleLogin"),
            ("rg --regexp=needle .", "needle"),
            ("rg --regexp needle .", "needle"),
            ("grep -efoo src", "foo"),
            ("grep -A 3 -i 'needle' src", "needle"),
            ("grep -A3 needle src", "needle"),
            ("rg -g '*.rs' needle src", "needle"),
            ("rg -tpy needle src", "needle"),
            ("rg --type rust needle src", "needle"),
            ("grep -rn --include=*.rs needle .", "needle"),
            ("grep -rn needle src 2>/dev/null", "needle"),
            ("grep -rn 2> /dev/null needle src", "needle"),
            ("cat x | grep -v foo", "foo"),
            ("cd src && rg needle | head -5", "needle"),
            ("rg -- -dash src", "-dash"),
            ("LC_ALL=C /usr/bin/grep -n needle src", "needle"),
        ] {
            assert_eq!(searched(command).as_deref(), Some(pattern), "{command}");
        }
        // No pattern on the command line, or no search at all: no search.
        for command in [
            "grep -f patterns.txt src",
            "rg --files src",
            "echo grep foo",
            "git commit -m \"grep the bug\"",
            "cargo build",
        ] {
            assert_eq!(searched(command), None, "{command}");
        }
    }

    /// `basename "$FILE_PATH"` read a path starting with `-` as options and
    /// produced no search term; `basename --` keeps it a path.
    #[test]
    #[cfg(unix)]
    fn shipped_file_hooks_take_a_dash_path_as_a_path() {
        if !jq_available() {
            eprintln!("skipping: jq not on PATH");
            return;
        }
        let hook = ShippedHook::new();
        for script in ["pre-read-enrich.sh", "post-edit-impact.sh"] {
            let event = serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
                "tool_input": {"file_path": "--db=x.py"}
            });
            let argv = hook
                .run(script, &event)
                .unwrap_or_else(|| panic!("{script} never called nestweaver"));
            assert_eq!(
                argv.last().map(String::as_str),
                Some("--db=x"),
                "{script}: {argv:?}"
            );
        }
    }

    /// One failing tool used to abort the whole run: tools after it were never
    /// attempted. Now every selected tool is attempted, the failure says what
    /// was already written, and the run still fails.
    #[test]
    fn a_failing_tool_does_not_stop_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        // A hand-written AGENTS.md makes Codex refuse AFTER .codex/config.toml.
        std::fs::write(base.join("AGENTS.md"), "# Hand written\n").unwrap();
        let db = tmp.path().join("t.lbug");
        let ctx = test_ctx(&db, &base);
        let tools = vec![
            ToolSetup {
                name: "codex",
                detected: true,
            },
            ToolSetup {
                name: "cursor",
                detected: true,
            },
        ];

        let error = run_setup_for_tools(None, &ctx, false, tools).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("setup failed for Codex"), "{message}");
        assert!(message.contains("1 other tool"), "{message}");
        assert!(
            base.join(".cursor/mcp.json").exists(),
            "the tool after the failure must still be configured"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("AGENTS.md")).unwrap(),
            "# Hand written\n"
        );
    }

    #[test]
    fn unknown_and_undetected_tools_get_distinct_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("t.lbug");
        let unknown = run_setup(Some("cursr"), &db, false, false, false, tmp.path())
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown tool 'cursr'"), "{unknown}");

        let ctx = test_ctx(&db, tmp.path());
        let tools = vec![ToolSetup {
            name: "cursor",
            detected: false,
        }];
        let undetected = run_setup_for_tools(Some("cursor"), &ctx, false, tools)
            .unwrap_err()
            .to_string();
        assert!(
            undetected.contains("Cursor was not detected"),
            "{undetected}"
        );
        assert!(undetected.contains("--all"), "{undetected}");
        assert!(!undetected.contains("valid options"), "{undetected}");
    }

    fn args_of(path: &Path, pointer: &str) -> Vec<String> {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        value
            .pointer(pointer)
            .and_then(|a| a.as_array())
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect()
    }

    /// Re-running setup with another `--db` (even with `--force`) kept the
    /// old database and said "already configured".
    #[test]
    fn a_different_db_is_reported_without_force_and_rebound_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"nestweaver":{"command":"nestweaver","args":["mcp","--lite","--db","/old.lbug"]}}}"#,
        )
        .unwrap();
        let desired = serde_json::json!({
            "command": "nestweaver",
            "args": ["mcp", "--db", "/new.lbug", "--config", "/new/instance.toml"]
        });
        let before = std::fs::read_to_string(&path).unwrap();

        let outcome = merge_json_mcp(
            &path,
            "mcpServers",
            "nestweaver",
            &desired,
            false,
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            Registration::PointsElsewhere {
                existing_db: "/old.lbug".into()
            }
        );
        assert!(outcome.status("x").contains("--force"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(outcome.effective_db("/new.lbug"), "/old.lbug");

        let outcome = merge_json_mcp(
            &path,
            "mcpServers",
            "nestweaver",
            &desired,
            true,
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            Registration::Rebound {
                previous_db: "/old.lbug".into()
            }
        );
        assert_eq!(
            args_of(&path, "/mcpServers/nestweaver/args"),
            [
                "mcp",
                "--lite",
                "--db",
                "/new.lbug",
                "--config",
                "/new/instance.toml"
            ],
            "--force rebinds the database and keeps the user's other flags; \
             --config is added only once the entry is this database's"
        );
    }

    /// An older relative registration names the same database as the absolute
    /// path setup now writes: not "points elsewhere".
    #[test]
    fn a_relative_entry_for_the_same_db_is_not_reported_as_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"nestweaver":{"command":"nestweaver","args":["mcp","--db","./nestweaver.lbug"]}}}"#,
        )
        .unwrap();
        let abs = dir.path().join("nestweaver.lbug").display().to_string();
        let desired = serde_json::json!({"command": "nestweaver", "args": ["mcp", "--db", abs]});
        assert_eq!(
            merge_json_mcp(
                &path,
                "mcpServers",
                "nestweaver",
                &desired,
                false,
                dir.path()
            )
            .unwrap(),
            Registration::Unchanged
        );
    }

    #[test]
    fn codex_rebinds_the_db_in_place_only_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\n    \"mcp\",\n    \"--db\",\n    \"/old.lbug\", # keep this comment\n]\n";
        std::fs::write(&path, original).unwrap();
        let desired = "\n[mcp_servers.nestweaver]\ncommand = \"nestweaver\"\nargs = [\"mcp\", \"--db\", \"/new.lbug\"]\n";

        assert!(matches!(
            merge_codex_mcp(&path, desired, false, dir.path()).unwrap(),
            Registration::PointsElsewhere { .. }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        assert!(matches!(
            merge_codex_mcp(&path, desired, true, dir.path()).unwrap(),
            Registration::Rebound { .. }
        ));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"/new.lbug\""), "{written}");
        assert!(!written.contains("/old.lbug"), "{written}");
        assert!(written.contains("keep this comment"), "{written}");
    }

    /// Every registration carries an absolute database path, so a host that
    /// launches the server from another directory — or a global config such as
    /// Windsurf's — still finds it.
    #[test]
    fn registrations_carry_an_absolute_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        run_setup(
            Some("cursor"),
            Path::new("./relative.lbug"),
            false,
            false,
            false,
            tmp.path(),
        )
        .unwrap();
        let args = args_of(
            &tmp.path().join(".cursor/mcp.json"),
            "/mcpServers/nestweaver/args",
        );
        let db = &args[args.iter().position(|a| a == "--db").unwrap() + 1];
        assert!(Path::new(db).is_absolute(), "{db}");
        assert!(!db.contains("/./"), "{db}");
    }

    /// VS Code's `.vscode/mcp.json` keys servers under `servers`; Copilot in
    /// VS Code reads the same file. Continue reads `.continue/mcpServers/`.
    #[test]
    fn host_formats_match_what_the_hosts_read() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("t.lbug");
        let ctx = test_ctx(&db, tmp.path());
        for tool in ["vscode", "copilot", "continue"] {
            configure_tool(tool, &ctx, &mut Items::new()).unwrap();
        }
        let vscode: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".vscode/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(vscode["servers"]["nestweaver"]["type"], "stdio", "{vscode}");
        assert!(vscode.get("mcpServers").is_none(), "{vscode}");
        assert!(!tmp.path().join(".github/copilot-mcp.json").exists());
        assert!(tmp.path().join(".github/copilot-instructions.md").exists());
        assert!(
            args_of(
                &tmp.path().join(".continue/mcpServers/nestweaver.json"),
                "/mcpServers/nestweaver/args"
            )
            .contains(&"--db".to_string())
        );
        for tool in ["vscode", "copilot", "continue"] {
            configured_mcp_entry(tool, tmp.path()).unwrap();
        }
    }

    /// Kiro reads `.kiro/settings/mcp.json`; OpenCode reads `opencode.json`
    /// with servers under `mcp` and the argv in a `command` array; Cline has
    /// no project-level file, so nothing is written for it.
    #[test]
    fn kiro_opencode_and_cline_follow_their_documented_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("t.lbug");
        let ctx = test_ctx(&db, tmp.path());
        for tool in ["kiro", "opencode"] {
            configure_tool(tool, &ctx, &mut Items::new()).unwrap();
        }
        assert!(
            args_of(
                &tmp.path().join(".kiro/settings/mcp.json"),
                "/mcpServers/nestweaver/args"
            )
            .contains(&"--db".to_string())
        );
        assert!(!tmp.path().join(".kiro/settings.json").exists());

        let opencode: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        let entry = &opencode["mcp"]["nestweaver"];
        assert_eq!(entry["type"], "local", "{opencode}");
        assert_eq!(entry["command"][0], "nestweaver", "{opencode}");
        assert_eq!(entry["command"][1], "mcp", "{opencode}");
        assert!(entry.get("args").is_none(), "{opencode}");
        assert!(!tmp.path().join(".opencode/config.json").exists());
        // Read back in the probe's command/args shape.
        let normalized = configured_mcp_entry("opencode", tmp.path()).unwrap();
        assert_eq!(normalized["command"], "nestweaver");
        assert_eq!(normalized["args"][0], "mcp");

        // A re-run reconciles the same entry instead of adding another.
        let mut items = Items::new();
        configure_tool("opencode", &ctx, &mut items).unwrap();
        assert_eq!(items[0].1, "already configured", "{items:?}");

        let mut items = Items::new();
        configure_tool("cline", &ctx, &mut items).unwrap();
        assert!(!tmp.path().join(".cline").exists());
        assert!(items[0].1.contains("Configure MCP Servers"), "{items:?}");
        assert!(items[0].1.contains("\"nestweaver\""), "{items:?}");
    }

    /// A repository can ship `.continue -> <somewhere outside>`; following it
    /// made setup create directories and write files outside the project.
    #[test]
    #[cfg(unix)]
    fn a_directory_link_leading_outside_the_project_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, project.join(".continue")).unwrap();
        let db = tmp.path().join("t.lbug");

        let error =
            configure_tool("continue", &test_ctx(&db, &project), &mut Items::new()).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside the project"),
            "{error:#}"
        );
        // The message is one sentence per line of source: a string
        // continuation without its trailing `\` kept every line's indentation
        // as a run of spaces in the middle of the text.
        assert!(
            !format!("{error:#}").contains("  "),
            "runs of spaces in the refusal: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("NestWeaver changed nothing for this tool"),
            "{error:#}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing may be created through the link"
        );

        // A link that stays inside the project is fine.
        let inside = tmp.path().join("project/real-kiro");
        std::fs::create_dir_all(&inside).unwrap();
        std::os::unix::fs::symlink(&inside, project.join(".kiro")).unwrap();
        configure_tool("kiro", &test_ctx(&db, &project), &mut Items::new()).unwrap();
        assert!(inside.join("settings/mcp.json").exists());
    }

    #[test]
    #[cfg(unix)]
    fn final_config_link_outside_project_is_refused_before_any_tool_write() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let external = tmp.path().join("global.json");
        std::fs::write(&external, "{}").unwrap();
        std::os::unix::fs::symlink(&external, project.join(".mcp.json")).unwrap();
        let db = tmp.path().join("t.lbug");
        let error =
            configure_tool("claude-code", &test_ctx(&db, &project), &mut Items::new()).unwrap_err();
        assert!(error.to_string().contains("outside the project"));
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "{}");
        assert!(!project.join(".claude").exists());
        std::fs::remove_file(project.join(".mcp.json")).unwrap();
        std::fs::write(project.join("real.json"), "{}").unwrap();
        std::os::unix::fs::symlink("real.json", project.join(".mcp.json")).unwrap();
        configure_tool("claude-code", &test_ctx(&db, &project), &mut Items::new()).unwrap();
        assert!(
            std::fs::read_to_string(project.join("real.json"))
                .unwrap()
                .contains("nestweaver")
        );
    }

    /// Only what older releases appended is removed; CRLF endings and an
    /// unrelated copy of the comment survive.
    #[test]
    fn aider_cleanup_removes_only_the_legacy_block() {
        let original = "# NestWeaver code intelligence\r\nmodel: sonnet\r\n\r\n# NestWeaver code intelligence\nrepo-map: nestweaver mcp --db ./x.lbug\n";
        assert_eq!(
            remove_legacy_aider_block(original),
            "# NestWeaver code intelligence\r\nmodel: sonnet\r\n"
        );
    }

    /// `repo-map:` is not an Aider option. Setup no longer writes it and
    /// removes the two lines older releases appended, leaving the rest.
    #[test]
    fn aider_legacy_lines_are_removed_and_nothing_new_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".aider.conf.yml");
        std::fs::write(
            &path,
            "model: sonnet\n\n# NestWeaver code intelligence\nrepo-map: nestweaver mcp --db ./nestweaver.lbug\n",
        )
        .unwrap();
        let db = tmp.path().join("t.lbug");
        let mut items = Items::new();
        configure_tool("aider", &test_ctx(&db, tmp.path()), &mut items).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model: sonnet\n");
        assert!(items[0].1.contains("removed"), "{items:?}");

        let fresh = tempfile::tempdir().unwrap();
        configure_tool("aider", &test_ctx(&db, fresh.path()), &mut Items::new()).unwrap();
        assert!(!fresh.path().join(".aider.conf.yml").exists());
    }
}

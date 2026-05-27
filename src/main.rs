mod setup;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use miette::Diagnostic;
use nestweaver_engine::{
    BrainContextResult, BrainWatcher, CodeWatcher, ContextResult, DeadCodeConfidence,
    FeatureContextResult, HybridSearchConfig, LookupResult, Summary, SummaryLevel,
    analyze_blast_radius, attach_cluster_ids, attach_communities,
    build_brain_context_hybrid_with_aliases, build_context_with_intent, build_feature_context,
    changed_files_from_git, compute_clusters, detect_dead_code, discover_cross_domain_links,
    embedding::generate_embedding, export_cypher, export_graphml, export_mermaid, filter_by_target,
    find_bridge_nodes, find_hub_nodes, generate_agents_md, generate_cursor_rule, generate_guide,
    generate_repo_map, generate_skill, generate_summaries, get_last_indexed_at, incremental_index,
    index_directory, index_markdown_directory, index_markdown_directory_since, list_repos,
    list_services, load_alias_sidecar, load_clusters, load_extensions, load_manifest_cache,
    lookup_symbol, record_last_indexed_at, render_text, save_clusters, save_summaries,
    search_symbols, suggest_links, truncate_to_budget,
};
use nestweaver_schema::Symbol;
use nestweaver_store::{GraphScope, GraphStore, QueryIntent, TantivyIndex};

// ── Exit codes ────────────────────────────────────────────────────────────────
const EXIT_SUCCESS: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_NOT_FOUND: i32 = 2;
const EXIT_AMBIGUOUS: i32 = 3;

// ── Rich diagnostics ─────────────────────────────────────────────────────────

/// CLI-layer diagnostic that wraps common `anyhow` errors with actionable help
/// text. Only used at the CLI boundary; library crates still use `thiserror`.
#[derive(Debug, Diagnostic, thiserror::Error)]
enum CliDiagnostic {
    #[error("Database not found: {path}")]
    #[diagnostic(
        code(nestweaver::db_not_found),
        help("Run `nestweaver index --repo <path>` to create a database")
    )]
    DatabaseNotFound { path: String },

    #[error("Repository path does not exist: {path}")]
    #[diagnostic(
        code(nestweaver::repo_not_found),
        help("Check that the path exists: {path}")
    )]
    RepoPathNotFound { path: String },

    #[error("No symbols found in the database")]
    #[diagnostic(
        code(nestweaver::empty_graph),
        help("Try indexing first with `nestweaver index --repo .`")
    )]
    NoSymbolsFound,

    #[error("Database is empty")]
    #[diagnostic(
        code(nestweaver::empty_db),
        help("Index a repository first: `nestweaver index --repo <path>`")
    )]
    EmptyDatabase,

    #[error("{message}")]
    #[diagnostic(code(nestweaver::error))]
    General { message: String },
}

/// Inspect an `anyhow::Error` and, when it matches a known pattern, convert it
/// into a `miette::Report` with rich diagnostic information (help text, error
/// code). Falls back to a plain `miette::Report` for unrecognised errors.
fn into_diagnostic(err: anyhow::Error) -> miette::Report {
    let message = format!("{err:#}");
    let lower = message.to_lowercase();

    if (lower.contains("no such file") || lower.contains("not found"))
        && (lower.contains("database") || lower.contains(".lbug"))
    {
        // Extract the path from common anyhow context patterns like
        // "failed to open database at ./foo.lbug: No such file ..."
        let path = message
            .split("at ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .unwrap_or("./nestweaver.lbug")
            .trim()
            .to_string();
        return CliDiagnostic::DatabaseNotFound { path }.into();
    }

    if lower.contains("path") && lower.contains("does not exist")
        || lower.contains("not a directory")
        || (lower.contains("no such file") && lower.contains("repo"))
    {
        let path = message
            .split(": ")
            .last()
            .unwrap_or(&message)
            .trim()
            .to_string();
        return CliDiagnostic::RepoPathNotFound { path }.into();
    }

    if lower.contains("no symbols found") || lower.contains("no matching symbols") {
        return CliDiagnostic::NoSymbolsFound.into();
    }

    if lower.contains("database is empty") || (lower.contains("empty") && lower.contains("graph")) {
        return CliDiagnostic::EmptyDatabase.into();
    }

    CliDiagnostic::General { message }.into()
}

// ── CLI structure ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "nestweaver",
    version,
    about = "Code knowledge graph for AI agents",
    long_about = "NestWeaver builds structural knowledge graphs of codebases and serves them\n\
                  to AI agents through query commands. Index a repo, then search symbols,\n\
                  trace dependencies, and generate context-window-sized summaries.\n\n\
                  Quick start:\n  \
                  nestweaver index --repo ./my-project\n  \
                  nestweaver context processPayment CheckoutService\n  \
                  nestweaver search \"UserService\"\n  \
                  nestweaver symbol \"processPayment\"\n  \
                  nestweaver repo-map --token-budget 2000",
    after_help = "Supported languages: JavaScript, TypeScript, Java, Go, Python, C, C++, Rust, C#, Kotlin, PHP, Ruby, Dart, Swift, COBOL\n\
                  Default database: ./nestweaver.lbug\n\n\
                  Shell completions:\n  \
                  nestweaver completions bash > ~/.local/share/bash-completion/completions/nestweaver\n  \
                  nestweaver completions zsh > ~/.zfunc/_nestweaver\n  \
                  nestweaver completions fish > ~/.config/fish/completions/nestweaver.fish"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Print timing and statistics after operations
    #[arg(long, global = true)]
    stats: bool,

    /// Suppress non-essential output
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

    /// Show additional detail (e.g. UIDs)
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    no_color: bool,

    /// Alias for --no-color (plain text output)
    #[arg(long, global = true)]
    plain: bool,
}

// ── Output configuration ─────────────────────────────────────────────────────

struct OutputConfig {
    quiet: bool,
    verbose: bool,
    /// Whether colored output is enabled. Reserved for future use by
    /// owo-colors formatting paths.
    #[allow(dead_code)]
    color: bool,
}

impl OutputConfig {
    fn from_cli(cli: &Cli) -> Self {
        let color = if cli.no_color || cli.plain || std::env::var("NO_COLOR").is_ok() {
            false
        } else {
            std::io::stderr().is_terminal()
        };
        Self {
            quiet: cli.quiet,
            verbose: cli.verbose,
            color,
        }
    }

    /// Print a progress/status message to stderr, unless `--quiet` is set.
    fn status(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }
}

fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

#[derive(Subcommand)]
enum Commands {
    /// List all indexed repositories
    ListRepos {
        #[arg(long, help = "Filter by instance ID")]
        instance: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List all services/modules in the graph
    ListServices {
        #[arg(long, help = "Filter by instance ID")]
        instance: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show a service summary with entry points
    ServiceSummary {
        /// Service name or UID to look up
        name: String,
        #[arg(long, help = "Filter by instance ID")]
        instance: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Search symbols by name substring
    #[command(
        after_help = "Examples:\n  nestweaver search \"User\"\n  nestweaver search \"process\" --limit 20 --json"
    )]
    Search {
        /// Text to search for in symbol names
        query: String,
        #[arg(long, default_value = "10", help = "Maximum number of results")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Look up a symbol by name or UID
    ///
    /// Returns the symbol's signature, file location, callers, and callees.
    /// If the name matches multiple symbols, prints candidates with UIDs.
    #[command(
        after_help = "Exit codes:\n  0  Found\n  2  Not found\n  3  Ambiguous (multiple matches)"
    )]
    Symbol {
        /// Symbol name or UID (use UID to disambiguate)
        name_or_uid: String,
        #[arg(long, help = "Filter by instance ID")]
        instance: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Analyze blast radius: what depends on this symbol
    ///
    /// Traverses incoming CALLS, IMPORTS, EXTENDS, and IMPLEMENTS edges
    /// to find all symbols that would be affected by a change.
    #[command(
        after_help = "Examples:\n  nestweaver impact \"processPayment\" --depth 5\n  nestweaver impact \"sym:repo:...:abc:42\" --confidence 0.8 --json"
    )]
    Impact {
        /// Symbol name or UID to analyze
        name_or_uid: String,
        #[arg(long, default_value = "3", help = "Maximum traversal depth")]
        depth: u32,
        #[arg(
            long,
            default_value = "0.0",
            help = "Minimum edge confidence [0.0-1.0]"
        )]
        confidence: f32,
        #[arg(long, help = "Filter by instance ID")]
        instance: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Generate a structural skeleton ranked by symbol importance
    ///
    /// Outputs the highest-PageRank symbols organized by file, truncated
    /// to fit within the specified token budget. Designed for AI agent
    /// context windows.
    RepoMap {
        #[arg(
            long,
            default_value = "4096",
            help = "Approximate token limit for output"
        )]
        token_budget: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show cross-repo references for a symbol
    CrossRepoRefs {
        /// Symbol name or UID
        name_or_uid: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Pull repository source into a local workspace
    ///
    /// By default uses sparse checkout (only fetches needed files).
    /// Source lands in the configured workspace directory.
    #[command(
        after_help = "Exit codes:\n  0  Success\n  4  Unauthorized (no access)\n  5  Unavailable (network/not found)"
    )]
    Pull {
        /// Repository URL to pull
        repo: String,
        #[arg(long, help = "Full clone instead of sparse checkout")]
        full: bool,
        #[arg(long, help = "Check out the exact SHA from the index (not HEAD)")]
        pinned: bool,
        #[arg(long, help = "Delete the checkout after use")]
        ephemeral: bool,
        #[arg(long, help = "Instance ID for workspace/credential config")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Index a local repository into the code graph
    ///
    /// Parses all supported source files, resolves cross-file references,
    /// computes PageRank, and stores the result in a LadybugDB database.
    #[command(
        after_help = "Examples:\n  nestweaver index --repo ./my-project\n  nestweaver index --repo ./my-project --db ./custom.lbug"
    )]
    Index {
        #[arg(long, help = "Path to the local repository to index")]
        repo: Option<PathBuf>,
        #[arg(long, help = "Instance ID (for multi-instance setups)")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the output database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Force full re-index, bypassing incremental detection")]
        force: bool,
    },
    /// Get task-focused context: structural subgraph around seed symbols
    ///
    /// Pass symbol names, UIDs, or file paths. Returns connected symbols
    /// ranked by structural relevance via Personalized PageRank.
    /// Use --feature with --config to resolve a declared feature bundle instead.
    #[command(
        after_help = "Examples:\n  nestweaver context processPayment CheckoutService\n  nestweaver context src/checkout/payment.ts\n  nestweaver context sym:repo:...:abc:42\n  nestweaver context --feature device-pairing --config ./instance.toml"
    )]
    Context {
        /// Symbol names, UIDs, or file paths to seed from (not required when --feature is set)
        #[arg(required_unless_present = "feature")]
        seeds: Vec<String>,
        #[arg(long, help = "Feature name from instance config")]
        feature: Option<String>,
        #[arg(long, help = "Path to instance config file (required with --feature)")]
        config: Option<PathBuf>,
        #[arg(
            long,
            help = "Query intent override: find-definition, understand-architecture, analyze-impact, general-context"
        )]
        intent: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List declared cross-repo links from an instance config
    #[command(after_help = "Examples:\n  nestweaver list-links --config ./instance.toml")]
    ListLinks {
        #[arg(long, help = "Path to instance config file")]
        config: PathBuf,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// List declared feature bundles from an instance config
    #[command(after_help = "Examples:\n  nestweaver list-features --config ./instance.toml")]
    ListFeatures {
        #[arg(long, help = "Path to instance config file")]
        config: PathBuf,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Analyze indexed repos and suggest cross-repo links and feature bundles
    #[command(after_help = "Examples:\n  nestweaver suggest-links --db ./all-repos.lbug")]
    SuggestLinks {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Manage NestWeaver instances (register, list, remove, pull)
    Instance {
        #[command(subcommand)]
        command: InstanceCommands,
    },
    /// Brain: unified knowledge graph over markdown vaults (walking skeleton)
    ///
    /// First step toward the Project Brain — indexes a markdown vault into the
    /// graph alongside any code repositories already indexed. Headings, sections,
    /// wikilinks, and PPR-based retrieval arrive in later phases.
    Brain {
        #[command(subcommand)]
        command: Box<BrainCommands>,
    },
    /// Run the brain as a Model Context Protocol server on stdio.
    ///
    /// Intended to be launched by Claude Desktop / Claude Code / Cowork via
    /// the MCP client configuration. The server stays alive, reading JSON-RPC
    /// requests from stdin and writing responses to stdout (one frame per
    /// line). All logs go to stderr — never write to stdout from any other
    /// code path while this is running.
    Mcp {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(
            long,
            help = "Allow the brain_add_source MCP tool to index new paths at runtime"
        )]
        allow_mcp_add_sources: bool,
        #[arg(
            long,
            help = "Expose only 6 core tools (for tools with limited tool slots like Cursor)"
        )]
        lite: bool,
    },
    /// Start the web UI server with interactive graph visualization
    Ui {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,

        #[arg(long, default_value = "3000", help = "Port to listen on")]
        port: u16,

        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,

        #[arg(long, help = "Do not open the browser automatically")]
        no_open: bool,
    },
    /// Generate an AGENTS.md codebase intelligence guide from the indexed graph.
    GenerateGuide {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Write to file instead of stdout")]
        output: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to instance config (TOML) — enriches guide with features and declared links"
        )]
        config: Option<PathBuf>,
        #[arg(
            long,
            default_value = "markdown",
            help = "Output format: markdown (default), skill, cursor-rule, agents-md"
        )]
        format: String,
    },
    /// Manage graph snapshots (build, verify, push)
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommands,
    },
    /// Show the most connected hub nodes in the code graph
    ///
    /// Hub nodes have the highest degree centrality (most incoming + outgoing
    /// edges) and tend to be central abstractions that many parts of the
    /// codebase depend on. Useful for understanding the architectural core.
    #[command(after_help = "Examples:\n  nestweaver hubs\n  nestweaver hubs --top 20 --json")]
    Hubs {
        #[arg(long, default_value = "10", help = "Number of top hubs to show")]
        top: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show architectural bridge/chokepoint nodes in the code graph
    ///
    /// Bridge nodes have high betweenness centrality: many shortest paths
    /// between other nodes pass through them. Changing a bridge node has
    /// outsized blast radius. Useful for identifying fragile connectors.
    #[command(after_help = "Examples:\n  nestweaver bridges\n  nestweaver bridges --top 20 --json")]
    Bridges {
        #[arg(long, default_value = "10", help = "Number of top bridges to show")]
        top: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List detected code communities (Leiden clustering)
    ///
    /// Runs community detection on the code graph and prints a summary of
    /// each cluster. Results are cached in a sidecar file alongside the
    /// database so subsequent invocations are instant.
    #[command(
        after_help = "Examples:\n  nestweaver clusters\n  nestweaver clusters --resolution 0.5 --json"
    )]
    Clusters {
        #[arg(
            long,
            default_value = "1.0",
            help = "Resolution parameter (higher = smaller clusters)"
        )]
        resolution: f64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Generate hierarchical code summaries for token-efficient retrieval
    ///
    /// Creates deterministic, compact summaries at three levels: symbol
    /// (function/class details), file (exports and imports), and cluster
    /// (community architecture). No LLM needed -- summaries are derived
    /// entirely from graph data.
    #[command(
        after_help = "Examples:\n  nestweaver summary --level symbol\n  nestweaver summary --level file --json\n  nestweaver summary --level cluster --token-budget 2000"
    )]
    Summary {
        #[arg(
            long,
            default_value = "file",
            help = "Summary level: symbol, file, or cluster"
        )]
        level: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Approximate token limit for output")]
        token_budget: Option<usize>,
        #[arg(
            long,
            help = "Filter to a specific target (file path, symbol name, or cluster name)"
        )]
        target: Option<String>,
    },
    /// Show details for a specific cluster
    ///
    /// Look up a cluster by its numeric ID or by (prefix of) its name.
    /// Prints the full member list and key files.
    #[command(
        after_help = "Examples:\n  nestweaver cluster 0\n  nestweaver cluster src/auth --json"
    )]
    Cluster {
        /// Cluster ID (number) or name prefix
        id_or_name: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List all projects from the store
    #[command(
        after_help = "Examples:\n  nestweaver list-projects\n  nestweaver list-projects --json"
    )]
    ListProjects {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Get context for a project: all notes and symbols, ranked by PPR
    #[command(
        after_help = "Examples:\n  nestweaver project-context my-project\n  nestweaver project-context my-project --token-budget 4000 --json"
    )]
    ProjectContext {
        /// Project name, alias, or UID
        name: String,
        #[arg(
            long,
            default_value = "3000",
            help = "Approximate token budget for the output"
        )]
        token_budget: usize,
        #[arg(long, help = "Also include notes/symbols from component sub-projects")]
        include_components: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        /// ISO 8601 timestamp. Only return Note/Section nodes modified after this time.
        /// Symbol nodes are always kept.
        #[arg(
            long = "since",
            help = "Hard filter: only Note/Section nodes modified after this ISO 8601 timestamp"
        )]
        since: Option<String>,
        /// Recency bias weight. 0 = disabled. 1.0 = same-day node ranks ~2x a year-old node.
        #[arg(
            long = "recency-weight",
            default_value = "0.0",
            help = "Multiplier for age-decay boost (default 0.0 = disabled)"
        )]
        recency_weight: f64,
        /// Half-life for the recency age-decay in days (default 30).
        #[arg(
            long = "recency-half-life-days",
            default_value = "30.0",
            help = "Half-life for age-decay in days (default 30.0)"
        )]
        recency_half_life_days: f64,
    },
    /// Auto-detect and configure NestWeaver for installed AI coding tools
    ///
    /// Detects Claude Code, Cursor, Codex, Windsurf, JetBrains, VS Code, Gemini CLI,
    /// GitHub Copilot, Aider, Kiro, Continue.dev, Cline, OpenCode, Trae, Devin, and Hermes,
    /// then writes the correct MCP server config and instruction files for each.
    #[command(
        after_help = "Examples:\n  nestweaver setup\n  nestweaver setup --all\n  nestweaver setup claude-code\n  nestweaver setup gemini\n  nestweaver setup copilot"
    )]
    Setup {
        /// Configure a specific tool (claude-code, cursor, codex, windsurf, jetbrains, vscode, gemini, copilot, aider, kiro, continue, cline, opencode, trae, devin, hermes)
        tool: Option<String>,
        /// Force-configure all tools even if not detected
        #[arg(long)]
        all: bool,
        /// Include --allow-mcp-add-sources in generated configs (enables set_extension writes)
        #[arg(long)]
        allow_writes: bool,
        /// Path to the NestWeaver database
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// Generate embeddings for all symbols in the database using an external API.
    ///
    /// Calls an OpenAI-compatible embedding endpoint for each symbol's signature
    /// text and stores the result so hybrid retrieval can use the semantic signal.
    /// Only symbols that do not yet have an embedding are processed (incremental).
    #[command(
        after_help = "Examples:\n  nestweaver embed --endpoint https://api.openai.com --model text-embedding-3-small\n  nestweaver embed --endpoint http://localhost:11434 --model nomic-embed-text --batch-size 8"
    )]
    Embed {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Embedding API endpoint (OpenAI-compatible)")]
        endpoint: String,
        #[arg(long, help = "Model name (e.g. text-embedding-3-small, voyage-code-3)")]
        model: String,
        #[arg(long, default_value = "32", help = "Batch size for API calls")]
        batch_size: usize,
    },

    /// Detect potentially dead code via entry point reachability
    ///
    /// Walks forward from every entry point following CALLS, IMPORTS,
    /// EXTENDS, IMPLEMENTS, and MEMBER_OF edges. Symbols not reached
    /// are reported as potentially dead, with confidence scoring based
    /// on visibility.
    #[command(
        after_help = "Examples:\n  nestweaver dead-code\n  nestweaver dead-code --min-confidence medium --json"
    )]
    DeadCode {
        #[arg(
            long,
            default_value = "low",
            help = "Minimum confidence to report (low, medium, high)"
        )]
        min_confidence: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// Export the code graph to an external format
    ///
    /// Supports Cypher (Neo4j), GraphML (Gephi/yEd), and Mermaid flowchart
    /// formats. Writes to stdout by default; use --output to write to a file.
    #[command(
        after_help = "Examples:\n  nestweaver export --format cypher\n  nestweaver export --format graphml --output graph.xml\n  nestweaver export --format mermaid --top 30"
    )]
    Export {
        #[arg(
            long,
            default_value = "cypher",
            help = "Output format: cypher, graphml, mermaid"
        )]
        format: String,
        #[arg(long, help = "Write to file instead of stdout")]
        output: Option<PathBuf>,
        #[arg(
            long,
            default_value = "50",
            help = "Number of top symbols for mermaid format (by PageRank)"
        )]
        top: usize,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// Generate shell completions for the given shell
    ///
    /// Prints completion scripts to stdout. Redirect to a file or source
    /// directly to enable tab completions for all nestweaver commands.
    #[command(after_help = "Examples:\n  \
          nestweaver completions bash > ~/.local/share/bash-completion/completions/nestweaver\n  \
          nestweaver completions zsh > ~/.zfunc/_nestweaver\n  \
          nestweaver completions fish > ~/.config/fish/completions/nestweaver.fish\n  \
          nestweaver completions powershell > nestweaver.ps1")]
    Completions {
        /// Shell to generate completions for (bash, zsh, fish, elvish, powershell)
        shell: clap_complete::Shell,
    },

    /// Analyze the blast radius of changed files in a PR or working tree
    ///
    /// Maps changed files to their symbols, runs transitive impact analysis,
    /// groups by cluster, and scores risk. When no --files are given, uses
    /// `git diff --name-only` to detect changed files automatically.
    #[command(
        after_help = "Examples:\n  nestweaver pr-impact\n  nestweaver pr-impact --files src/auth.rs,src/db.rs\n  nestweaver pr-impact --depth 5 --json"
    )]
    PrImpact {
        #[arg(
            long,
            help = "Comma-separated list of changed file paths (omit to auto-detect via git diff)"
        )]
        files: Option<String>,
        #[arg(long, default_value = "3", help = "Maximum traversal depth")]
        depth: u32,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// Watch a repository for source file changes and re-index incrementally
    ///
    /// Monitors the repo directory for creates, modifies, and deletes of
    /// supported source files. Changes are debounced into 2-second windows
    /// and each batch triggers an incremental re-index. Ctrl-C stops cleanly.
    #[command(
        after_help = "Examples:\n  nestweaver watch\n  nestweaver watch --repo ./my-project\n  nestweaver watch --repo ./my-project --db ./custom.lbug"
    )]
    Watch {
        #[arg(long, help = "Path to the local repository to watch")]
        repo: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Instance ID (for multi-instance setups)")]
        instance: Option<String>,
    },
}

#[derive(Subcommand)]
enum BrainCommands {
    /// Index a markdown vault into the brain. Auto-detects Obsidian vault
    /// (.obsidian/ present) vs plain markdown folder.
    Add {
        /// Path to the vault directory.
        path: PathBuf,
        #[arg(long, help = "Friendly name for the vault (default: directory name)")]
        name: Option<String>,
        #[arg(long, help = "Instance ID for multi-instance setups")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List all indexed vaults with their note counts.
    List {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show overall brain status: vault count, note count, source kinds.
    Status {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Watch a vault directory for changes and keep the brain in sync.
    /// Runs in the foreground; Ctrl-C stops it cleanly. On each .md save
    /// the changed file is re-parsed and its nodes are replaced via
    /// cascade-delete + re-insert.
    Watch {
        /// Vault directory to watch.
        path: PathBuf,
        #[arg(long, help = "Friendly name for the vault (default: directory name)")]
        name: Option<String>,
        #[arg(long, help = "Instance ID")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Force a full re-index of a vault. Drops the vault's existing notes
    /// from the graph (via cascade-delete) then re-runs the indexer from
    /// scratch. Use after a `git pull`, large bulk paste, or any change
    /// that the watcher may have missed.
    Refresh {
        /// Vault directory to refresh.
        path: PathBuf,
        #[arg(long, help = "Friendly name for the vault (default: directory name)")]
        name: Option<String>,
        #[arg(long, help = "Instance ID")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(
            long,
            help = "Only re-index files modified since this timestamp (ISO 8601, e.g. 2026-05-26T00:00:00Z)"
        )]
        since: Option<String>,
    },
    /// Remove a vault from the brain. Drops the Vault node and
    /// cascade-deletes every Note/Heading/Section/edge belonging to it.
    /// The on-disk vault files are NOT touched.
    Remove {
        /// Vault directory path (the same path passed to `brain add`).
        path: PathBuf,
        #[arg(long, help = "Instance ID [default: default]")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Rebuild the Tantivy BM25 search index from the current graph
    /// state. Use after a fresh `brain add`, or to recover from an
    /// out-of-sync sidecar.
    ReindexSearch {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Search notes, headings, and sections by keyword. Uses BM25
    /// full-text search when a Tantivy index exists, falls back to
    /// substring matching on note titles otherwise.
    #[command(
        after_help = "Examples:\n  nestweaver brain search stripe\n  nestweaver brain search \"payment flow\" --limit 5 --json"
    )]
    Search {
        /// Search query string.
        query: String,
        #[arg(long, default_value = "20", help = "Maximum results")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Unified PPR context across code + notes. Seeds may be note titles,
    /// tag names (with or without #), symbol names, or any UID
    /// (sym:/note:/head:/sec:/tag:/repo:/vlt:).
    Context {
        /// Seed strings to anchor the PPR walk.
        #[arg(required = true)]
        seeds: Vec<String>,
        /// Approximate token cap for the output (characters / 4). When set,
        /// truncates the connected list to fit. This is the primary knob
        /// for LLM context-window-sized output.
        #[arg(long, help = "Approximate token budget for the connected list")]
        token_budget: Option<usize>,
        /// Hard cap on connected results. Used when --token-budget is not
        /// set; ignored when it is.
        #[arg(long, default_value = "30", help = "Maximum connected results to show")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        /// Filter results to nodes whose kind starts with one of these values
        /// (e.g. Symbol, Note, Section, Tag, Heading).
        #[arg(
            long = "kinds",
            help = "Keep only nodes with these kind prefixes (e.g. Symbol, Note)"
        )]
        kinds: Vec<String>,
        /// Filter results to nodes associated with these repo UIDs or names.
        #[arg(long = "repos", help = "Keep only nodes from these repo UIDs or names")]
        repos: Vec<String>,
        /// Filter results to nodes associated with these vault UIDs or names.
        #[arg(
            long = "vaults",
            help = "Keep only nodes from these vault UIDs or names"
        )]
        vaults: Vec<String>,
        /// Keep only nodes whose location (file path) starts with this prefix.
        #[arg(
            long = "path-prefix",
            help = "Keep only nodes whose location starts with this prefix"
        )]
        path_prefix: Option<String>,
        /// Include only nodes tagged with any of these tags (note/section nodes only).
        #[arg(
            long = "tags",
            help = "Keep only note/section nodes tagged with any of these tags"
        )]
        tags: Vec<String>,
        /// Exclude nodes tagged with any of these tags (note/section nodes only).
        #[arg(
            long = "exclude-tags",
            help = "Exclude note/section nodes tagged with any of these tags"
        )]
        exclude_tags: Vec<String>,
        /// PPR ranking weight for hybrid RRF fusion (default 0.7).
        #[arg(
            long = "weight-ppr",
            help = "PPR weight for hybrid retrieval (default 0.7)"
        )]
        weight_ppr: Option<f64>,
        /// BM25 text search weight for hybrid RRF fusion (default 0.3).
        #[arg(
            long = "weight-bm25",
            help = "BM25 weight for hybrid retrieval (default 0.3)"
        )]
        weight_bm25: Option<f64>,
        /// Semantic embedding weight for hybrid RRF fusion (default 0.0).
        #[arg(
            long = "weight-semantic",
            help = "Semantic embedding weight for hybrid retrieval (default 0.0)"
        )]
        weight_semantic: Option<f64>,
        /// ISO 8601 timestamp. Only return Note/Section nodes modified after this time.
        /// Symbol nodes are always kept.
        #[arg(
            long = "since",
            help = "Hard filter: only Note/Section nodes modified after this ISO 8601 timestamp"
        )]
        since: Option<String>,
        /// Recency bias weight. 0 = disabled. 1.0 = same-day node ranks ~2x a year-old node.
        #[arg(
            long = "recency-weight",
            default_value = "0.0",
            help = "Multiplier for age-decay boost (default 0.0 = disabled)"
        )]
        recency_weight: f64,
        /// Half-life for the recency age-decay in days (default 30).
        #[arg(
            long = "recency-half-life-days",
            default_value = "30.0",
            help = "Half-life for age-decay in days (default 30.0)"
        )]
        recency_half_life_days: f64,
    },
}

#[derive(Subcommand)]
enum InstanceCommands {
    /// Register an instance from a TOML config file
    Register {
        /// Path to the instance config file (.toml)
        config_path: String,
    },
    /// List all registered instances
    List,
    /// Remove a registered instance
    Remove {
        /// Instance ID to remove
        id: String,
    },
    /// Pull the latest snapshot for an instance
    Pull {
        /// Instance ID to pull
        id: String,
    },
}

#[derive(Subcommand)]
enum SnapshotCommands {
    /// Build a snapshot from the current graph
    Build {
        #[arg(long, help = "Instance ID to build snapshot for")]
        instance: Option<String>,
    },
    /// Verify snapshot integrity (checksum, schema, version)
    Verify {
        /// Path to the snapshot directory
        path: String,
    },
    /// Push a snapshot to the configured storage backend
    Push {
        #[arg(long, help = "Instance ID to push snapshot for")]
        instance: Option<String>,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn default_db_path() -> PathBuf {
    if let Ok(env_db) = std::env::var("NESTWEAVER_DB") {
        PathBuf::from(env_db)
    } else {
        PathBuf::from("./nestweaver.lbug")
    }
}

fn detect_repo_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd,
        }
    }
}

fn resolve_index_db_path(db: Option<PathBuf>, repo_root: &Path) -> PathBuf {
    if let Some(explicit) = db {
        return explicit;
    }
    if let Ok(env_db) = std::env::var("NESTWEAVER_DB") {
        return PathBuf::from(env_db);
    }
    repo_root.join("nestweaver.lbug")
}

fn open_store(db: Option<&Path>) -> anyhow::Result<GraphStore> {
    let default = default_db_path();
    let path = db.unwrap_or(&default);
    let store = match GraphStore::open(path) {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("lock") || msg.contains("Lock") {
                eprintln!(
                    "Database is locked (another process is using it). \
                     Opening in read-only mode..."
                );
                GraphStore::open_read_only(path).with_context(|| {
                    format!(
                        "failed to open database at {} (read-only fallback also failed)",
                        path.display()
                    )
                })?
            } else {
                return Err(e)
                    .with_context(|| format!("failed to open database at {}", path.display()));
            }
        }
    };
    let pr_path = path.with_extension("pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);
    Ok(store)
}

/// Tantivy index sidecar location: `<db_path>.tantivy/`. Mirrors the
/// `.pagerank.json` sidecar convention.
fn tantivy_sidecar_path_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".tantivy");
    PathBuf::from(s)
}

fn default_registry_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nestweaver")
        .join("registry.json")
}

/// Parse a subset of ISO 8601 / RFC 3339 timestamps into a `SystemTime`.
///
/// Accepted forms (all interpreted as UTC):
/// - `2026-05-26T00:00:00Z`
/// - `2026-05-26T00:00:00+00:00`
/// - `2026-05-26T00:00:00`  (no offset — assumed UTC)
/// - `2026-05-26`            (date-only — midnight UTC)
///
/// Returns an error for anything that doesn't match these patterns.
fn parse_iso8601_to_system_time(s: &str) -> Result<std::time::SystemTime, anyhow::Error> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // Strip trailing Z or +00:00 / -00:00 offset (we always treat as UTC).
    let s = s.trim();
    let s = if let Some(rest) = s.strip_suffix('Z') {
        rest
    } else if let Some(rest) = s.strip_suffix("+00:00") {
        rest
    } else if let Some(rest) = s.strip_suffix("-00:00") {
        rest
    } else {
        s
    };

    // Split date and optional time at 'T'.
    let (date_part, time_part) = if let Some(pos) = s.find('T') {
        (&s[..pos], Some(&s[pos + 1..]))
    } else {
        (s, None)
    };

    // Parse date.
    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 {
        anyhow::bail!("expected YYYY-MM-DD, got '{date_part}'");
    }
    let year: i64 = date_parts[0].parse().context("year")?;
    let month: u32 = date_parts[1].parse().context("month")?;
    let day: u32 = date_parts[2].parse().context("day")?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        anyhow::bail!("out-of-range date: {date_part}");
    }

    // Parse optional time.
    let (hour, minute, second) = if let Some(t) = time_part {
        // Also strip any offset that was not caught above (e.g. +05:30).
        let t = if let Some(pos) = t.rfind(['+', '-']) {
            &t[..pos]
        } else {
            t
        };
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() < 2 {
            anyhow::bail!("expected HH:MM[:SS], got '{t}'");
        }
        let h: u32 = parts[0].parse().context("hour")?;
        let m: u32 = parts[1].parse().context("minute")?;
        let sec: u32 = if parts.len() >= 3 {
            // Truncate any fractional seconds.
            parts[2]
                .split('.')
                .next()
                .unwrap_or("0")
                .parse()
                .context("second")?
        } else {
            0
        };
        if h > 23 || m > 59 || sec > 60 {
            anyhow::bail!("out-of-range time: {t}");
        }
        (h, m, sec)
    } else {
        (0, 0, 0)
    };

    // Convert to Unix seconds using the same civil-to-days algorithm as
    // `secs_to_ymd_hms` in nestweaver-engine (Howard Hinnant).
    let unix_secs = {
        // Days since Unix epoch (1970-01-01).
        let m_adj = if month <= 2 { month + 9 } else { month - 3 };
        let y_adj = if month <= 2 { year - 1 } else { year };
        let era = if y_adj >= 0 {
            y_adj / 400
        } else {
            (y_adj - 399) / 400
        };
        let yoe = (y_adj - era * 400) as u64;
        let doy = (153 * m_adj as u64 + 2) / 5 + day as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = (era * 146_097 + doe as i64) - 719_468;
        days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
    };

    if unix_secs < 0 {
        // Pre-epoch: just return UNIX_EPOCH (nothing can be "older" than that).
        return Ok(UNIX_EPOCH);
    }
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs as u64))
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Install miette as the global error/panic report handler for rich
    // diagnostics (colours, help text, error codes) on supported terminals.
    miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().build())
    }))
    .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let out = OutputConfig::from_cli(&cli);
    let show_stats = cli.stats;

    let exit_code = match run(cli, &out) {
        Ok((code, summary)) => {
            if let (true, Some(s)) = (show_stats, summary) {
                eprintln!("stats: {s}");
            }
            code
        }
        Err(e) => {
            let report = into_diagnostic(e);
            eprintln!("{report:?}");
            EXIT_ERROR
        }
    };

    process::exit(exit_code);
}

fn run(cli: Cli, out: &OutputConfig) -> anyhow::Result<(i32, Option<String>)> {
    let t0 = std::time::Instant::now();
    let _ = &t0; // suppress unused warning for arms that don't use it
    match cli.command {
        Commands::ListRepos { instance, json, db } => {
            let store = open_store(db.as_deref())?;
            let repos = list_repos(&store, instance.as_deref())?;

            if json {
                println!("{}", serde_json::to_string_pretty(&repos)?);
            } else if repos.is_empty() {
                println!("No repositories found.");
            } else {
                for repo in &repos {
                    println!("{}", repo.uid);
                    println!("  URL:     {}", repo.url);
                    println!("  SHA:     {}", repo.indexed_sha);
                    println!("  Instance: {}", repo.instance_id);
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ListServices { instance, json, db } => {
            let store = open_store(db.as_deref())?;
            let services = list_services(&store, instance.as_deref())?;

            if json {
                println!("{}", serde_json::to_string_pretty(&services)?);
            } else if services.is_empty() {
                println!("No services found.");
            } else {
                for svc in &services {
                    println!("{}", svc.name);
                    println!("  UID:  {}", svc.uid);
                    println!("  Repo: {}", svc.repo_uid);
                    if let Some(summary) = &svc.summary {
                        println!("  Summary: {summary}");
                    }
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ServiceSummary {
            name,
            instance,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;
            let services = list_services(&store, instance.as_deref())?;
            let service = services.iter().find(|s| s.name == name || s.uid == name);
            match service {
                Some(s) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(s)?);
                    } else {
                        println!("Service: {}", s.name);
                        if let Some(ref summary) = s.summary {
                            println!("Summary: {summary}");
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                None => {
                    eprintln!("Service not found: {name}");
                    Ok((EXIT_NOT_FOUND, None))
                }
            }
        }

        Commands::RepoMap {
            token_budget,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;
            let map = generate_repo_map(&store, token_budget)?;
            let token_count = map.len().div_ceil(4);

            if json {
                #[derive(serde::Serialize)]
                struct RepoMapJson<'a> {
                    map: &'a str,
                    token_count: usize,
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&RepoMapJson {
                        map: &map,
                        token_count,
                    })?
                );
            } else {
                print!("{map}");
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::CrossRepoRefs {
            name_or_uid,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;
            match resolve_uid(&store, &name_or_uid)? {
                ResolveResult::Found(uid) => {
                    let refs = store
                        .cross_repo_links(&uid)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&refs)?);
                    } else if refs.is_empty() {
                        println!("No cross-repo references found for '{name_or_uid}'.");
                    } else {
                        println!(
                            "Cross-repo references for '{}' ({}):",
                            name_or_uid,
                            refs.len()
                        );
                        for r in &refs {
                            println!(
                                "  {} -> {} [{}] ({:.2})",
                                r.source_name, r.target_name, r.link_type, r.confidence
                            );
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                ResolveResult::NotFound => {
                    eprintln!("Symbol '{name_or_uid}' not found.");
                    Ok((EXIT_NOT_FOUND, None))
                }
                ResolveResult::Ambiguous(candidates) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&candidates)?);
                    } else {
                        eprintln!(
                            "Ambiguous: '{}' matches {} symbols:",
                            name_or_uid,
                            candidates.len()
                        );
                        for c in &candidates {
                            eprintln!("  {} [{}] {}:{}", c.uid, c.kind, c.file_path, c.start_line);
                        }
                    }
                    Ok((EXIT_AMBIGUOUS, None))
                }
            }
        }

        Commands::Pull {
            repo,
            full,
            pinned,
            ephemeral,
            instance: _,
            db,
        } => {
            let db_default = default_db_path();
            let db_path = db.as_deref().unwrap_or(&db_default);
            let store = nestweaver_store::GraphStore::open(db_path)
                .with_context(|| format!("failed to open database at {}", db_path.display()))?;

            let workspace_root = dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("nestweaver")
                .join("workspace");

            let mode = if full {
                nestweaver_engine::PullMode::Full
            } else {
                nestweaver_engine::PullMode::Sparse { files: vec![] }
            };

            let repos = store.list_repos(None)?;

            let sha_policy = if pinned {
                let indexed_sha = repos
                    .iter()
                    .find(|r| {
                        r.url == repo
                            || nestweaver_engine::repo_name_from_url(&r.url)
                                == nestweaver_engine::repo_name_from_url(&repo)
                    })
                    .map(|r| r.indexed_sha.clone())
                    .unwrap_or_default();
                nestweaver_engine::ShaPolicy::Pinned(indexed_sha)
            } else {
                nestweaver_engine::ShaPolicy::Head
            };

            let indexed_sha = repos
                .iter()
                .find(|r| {
                    r.url == repo
                        || nestweaver_engine::repo_name_from_url(&r.url)
                            == nestweaver_engine::repo_name_from_url(&repo)
                })
                .map(|r| r.indexed_sha.clone())
                .unwrap_or_default();

            match nestweaver_engine::pull_repo(
                &workspace_root,
                &repo,
                &indexed_sha,
                &nestweaver_engine::PullOptions {
                    mode,
                    sha_policy,
                    ephemeral,
                },
            ) {
                Ok(result) => {
                    println!("Pulled to {}", result.path.display());
                    if let Some(drift) = result.drift_commits
                        && drift > 0
                    {
                        eprintln!("Warning: index is {} commits behind HEAD", drift);
                    }
                    if ephemeral {
                        nestweaver_engine::cleanup_repo(&workspace_root, &repo)?;
                        println!("Ephemeral: cleaned up");
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                Err(e) => {
                    eprintln!("Pull failed: {e}");
                    Ok((e.exit_code(), None))
                }
            }
        }

        Commands::Context {
            seeds,
            feature,
            config,
            intent,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;

            let parsed_intent: Option<QueryIntent> = intent
                .as_deref()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid --intent value: {e}"))?;

            if let Some(feature_name) = &feature {
                // Feature-mode: resolve via instance config.
                let config_path = config
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--config is required when using --feature"))?;
                let instance_config = nestweaver_engine::InstanceConfig::from_file(config_path)?;
                let feature_config = instance_config
                    .features
                    .as_ref()
                    .and_then(|fs| fs.iter().find(|f| f.name == *feature_name))
                    .ok_or_else(|| {
                        anyhow::anyhow!("feature '{}' not found in config", feature_name)
                    })?;
                let empty_links = vec![];
                let links = instance_config.links.as_deref().unwrap_or(&empty_links);

                match build_feature_context(&store, feature_config, links) {
                    Ok(result) => {
                        let stats = format!(
                            "{} seeds, {} connected nodes in {}",
                            result.seeds.len(),
                            result.connected.len(),
                            format_elapsed(t0.elapsed())
                        );
                        if json {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        } else {
                            print_feature_context_text(&result);
                        }
                        Ok((EXIT_SUCCESS, Some(stats)))
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("No symbols found") {
                            eprintln!("{msg}");
                            Ok((EXIT_NOT_FOUND, None))
                        } else {
                            eprintln!("Error: {msg}");
                            Ok((EXIT_ERROR, None))
                        }
                    }
                }
            } else {
                // Normal seed-based context.
                match build_context_with_intent(&store, &seeds, parsed_intent) {
                    Ok(result) => {
                        let stats = format!(
                            "{} seeds, {} connected nodes in {}",
                            result.seeds.len(),
                            result.connected.len(),
                            format_elapsed(t0.elapsed())
                        );
                        if json {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        } else {
                            print_context_text(&result);
                        }
                        Ok((EXIT_SUCCESS, Some(stats)))
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("No matching symbols") {
                            eprintln!("{msg}");
                            Ok((EXIT_NOT_FOUND, None))
                        } else if msg.contains("Ambiguous") {
                            eprintln!("{msg}");
                            Ok((EXIT_AMBIGUOUS, None))
                        } else {
                            eprintln!("Error: {msg}");
                            Ok((EXIT_ERROR, None))
                        }
                    }
                }
            }
        }

        Commands::ListLinks { config, json } => {
            let instance_config = nestweaver_engine::InstanceConfig::from_file(&config)?;
            let links = instance_config.links.unwrap_or_default();

            if json {
                println!("{}", serde_json::to_string_pretty(&links)?);
            } else if links.is_empty() {
                println!("No links declared in config.");
            } else {
                for link in &links {
                    println!("{} → {}", link.from, link.to);
                    println!("  Type: {}", link.link_type);
                    if let Some(desc) = &link.description {
                        println!("  Description: {desc}");
                    }
                    if let Some(eps) = &link.endpoints {
                        println!("  Endpoints: {}", eps.join(", "));
                    }
                    if let Some(ids) = &link.identifiers {
                        println!("  Identifiers: {}", ids.join(", "));
                    }
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ListFeatures { config, json } => {
            let instance_config = nestweaver_engine::InstanceConfig::from_file(&config)?;
            let features = instance_config.features.unwrap_or_default();

            if json {
                println!("{}", serde_json::to_string_pretty(&features)?);
            } else if features.is_empty() {
                println!("No features declared in config.");
            } else {
                for feat in &features {
                    println!("{}", feat.name);
                    if let Some(desc) = &feat.description {
                        println!("  {desc}");
                    }
                    println!("  Repos: {}", feat.repos.join(", "));
                    println!("  Entry points: {}", feat.entry_points.join(", "));
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::SuggestLinks { db, json } => {
            let db_default = default_db_path();
            let db_path = db.as_deref().unwrap_or(&db_default);
            let store = open_store(Some(db_path))?;
            let cache_path = db_path.with_extension("manifests.json");
            let manifests = load_manifest_cache(&cache_path).unwrap_or_default();
            let suggestions = suggest_links(&store, &manifests)?;

            if json {
                #[derive(serde::Serialize)]
                struct SuggestJson<'a> {
                    links: &'a [nestweaver_engine::SuggestedLink],
                    features: &'a [nestweaver_engine::SuggestedFeature],
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&SuggestJson {
                        links: &suggestions.links,
                        features: &suggestions.features,
                    })?
                );
            } else {
                if suggestions.links.is_empty() && suggestions.features.is_empty() {
                    println!("No cross-repo connections detected.");
                    println!("Tip: Index multiple repos into the same database first.");
                    return Ok((EXIT_SUCCESS, None));
                }

                if !suggestions.links.is_empty() {
                    println!("# Suggested links (review and add to your instance config)\n");
                    for link in &suggestions.links {
                        println!("[[links]]");
                        println!("from = \"{}\"", link.from);
                        println!("to = \"{}\"", link.to);
                        println!("type = \"{}\"", link.link_type);
                        println!(
                            "description = \"{}\"",
                            link.description.replace('\\', "\\\\").replace('"', "\\\"")
                        );
                        println!(
                            "# Confidence: {} ({} shared symbols)",
                            link.confidence,
                            link.shared_symbols.len()
                        );
                        println!();
                    }
                }

                if !suggestions.features.is_empty() {
                    println!("# Suggested features (review and add to your instance config)\n");
                    for feat in &suggestions.features {
                        println!("[[features]]");
                        println!("name = \"{}\"", feat.name);
                        println!(
                            "description = \"{}\"",
                            feat.description.replace('\\', "\\\\").replace('"', "\\\"")
                        );
                        let repos_toml: Vec<String> =
                            feat.repos.iter().map(|r| format!("\"{r}\"")).collect();
                        println!("repos = [{}]", repos_toml.join(", "));
                        let eps_toml: Vec<String> = feat
                            .entry_points
                            .iter()
                            .map(|e| format!("\"{e}\""))
                            .collect();
                        println!("entry_points = [{}]", eps_toml.join(", "));
                        println!();
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::GenerateGuide {
            db,
            output,
            config,
            format,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;
            let instance_config = config
                .as_deref()
                .map(nestweaver_engine::InstanceConfig::from_file)
                .transpose()?;
            let output_str = match format.as_str() {
                "skill" => generate_skill(&store, instance_config.as_ref())?,
                "cursor-rule" => generate_cursor_rule(&store, instance_config.as_ref())?,
                "agents-md" => generate_agents_md(&store, instance_config.as_ref())?,
                _ => generate_guide(&store, instance_config.as_ref())?,
            };
            match output {
                Some(path) => {
                    std::fs::write(&path, &output_str)?;
                    out.status(&format!("Guide written to {}", path.display()));
                }
                None => print!("{output_str}"),
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Hubs { top, json, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;

            let mut hubs = find_hub_nodes(&store, top)?;

            // Attach cluster IDs if clustering sidecar exists.
            if let Ok(Some(clustering)) = load_clusters(&db_path) {
                attach_cluster_ids(&mut hubs, &clustering);
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&hubs)?);
            } else if hubs.is_empty() {
                println!("No hub nodes found (graph may be empty).");
            } else {
                println!("Top {} hub nodes (by total degree):\n", hubs.len());
                for h in &hubs {
                    let cluster = h
                        .cluster_id
                        .map(|id| format!(" cluster={id}"))
                        .unwrap_or_default();
                    println!(
                        "  {} ({}) in={} out={} total={} pr={:.4}{cluster}",
                        h.name,
                        h.file_path,
                        h.in_degree,
                        h.out_degree,
                        h.total_degree,
                        h.pagerank_score,
                    );
                }
            }
            let stats = format!("{} hubs in {}", hubs.len(), format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Bridges { top, json, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;

            out.status("Computing betweenness centrality...");
            let mut bridges = find_bridge_nodes(&store, top)?;

            // Attach community connection info if clustering sidecar exists.
            if let Ok(Some(clustering)) = load_clusters(&db_path) {
                attach_communities(&mut bridges, &clustering, &store);
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&bridges)?);
            } else if bridges.is_empty() {
                println!("No bridge nodes found (graph may be empty).");
            } else {
                println!(
                    "Top {} bridge nodes (by betweenness centrality):\n",
                    bridges.len()
                );
                for b in &bridges {
                    let communities = if b.communities_connected.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " connects=[{}]",
                            b.communities_connected
                                .iter()
                                .map(|c| c.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    };
                    println!(
                        "  {} ({}) betweenness={:.2}{communities}",
                        b.name, b.file_path, b.betweenness_score,
                    );
                }
            }
            let stats = format!(
                "{} bridges in {}",
                bridges.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Summary {
            level,
            json,
            db,
            token_budget,
            target,
        } => {
            let parsed_level: SummaryLevel =
                level.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;

            out.status(&format!("Generating {} summaries...", parsed_level));
            let summaries = generate_summaries(&store, parsed_level)?;

            // Save to sidecar for later use.
            save_summaries(&db_path, &summaries)?;

            // Optional target filter, then token budget truncation.
            let after_filter: Vec<Summary> = if let Some(ref t) = target {
                filter_by_target(&summaries, t)
                    .into_iter()
                    .cloned()
                    .collect()
            } else {
                summaries
            };

            let display: Vec<Summary> = if let Some(budget) = token_budget {
                truncate_to_budget(&after_filter, budget)
                    .into_iter()
                    .cloned()
                    .collect()
            } else {
                after_filter
            };

            if json {
                println!("{}", serde_json::to_string_pretty(&display)?);
            } else if display.is_empty() {
                println!("No summaries generated (graph may be empty).");
            } else {
                let text = render_text(&display);
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }

            let total_tokens: usize = display.iter().map(|s| s.token_estimate).sum();
            let stats = format!(
                "{} summaries ({} tokens) in {}",
                display.len(),
                total_tokens,
                format_elapsed(t0.elapsed()),
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Clusters {
            resolution,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // Compute and save inside a block so the store is dropped
            // before any output. LadybugDB's connection finaliser can
            // trigger a panic during WAL checkpoint; dropping early
            // converts that into a catchable error scope and keeps the
            // process exit clean.
            let output = {
                let store = open_store(Some(&db_path))?;
                out.status(&format!("Computing clusters (resolution={resolution})..."));
                let o = compute_clusters(&store, resolution)?;
                save_clusters(&db_path, &o)?;
                out.status(&format!(
                    "Found {} community(ies), modularity={:.4}. Saved to sidecar.",
                    o.communities.len(),
                    o.modularity
                ));
                // Drop `store` here, before output begins.
                o
            };

            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if output.communities.is_empty() {
                println!("No communities detected (graph may be empty or fully disconnected).");
            } else {
                println!(
                    "Clusters ({}, modularity={:.4}):\n",
                    output.communities.len(),
                    output.modularity
                );
                for c in &output.communities {
                    println!(
                        "  [{:>3}] {} ({} members, cohesion={:.2})",
                        c.id, c.name, c.member_count, c.cohesion
                    );
                    for f in &c.key_files {
                        println!("        {f}");
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Cluster {
            id_or_name,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // Load cached clusters from sidecar. If none exist, compute them.
            let output = match load_clusters(&db_path)? {
                Some(cached) => cached,
                None => {
                    out.status("No cached clusters found; computing with default resolution...");
                    let store = open_store(Some(&db_path))?;
                    let computed = compute_clusters(&store, 1.0)?;
                    save_clusters(&db_path, &computed)?;
                    computed
                }
            };

            // Find the matching community: try numeric ID first, then name prefix.
            let community = if let Ok(id) = id_or_name.parse::<u32>() {
                output.communities.iter().find(|c| c.id == id)
            } else {
                let needle = id_or_name.to_lowercase();
                output
                    .communities
                    .iter()
                    .find(|c| c.name.to_lowercase().starts_with(&needle))
            };

            match community {
                Some(c) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(c)?);
                    } else {
                        println!("Cluster [{}]: {}", c.id, c.name);
                        println!("  Members: {}  Cohesion: {:.4}", c.member_count, c.cohesion);
                        println!();
                        println!("  Key files:");
                        for f in &c.key_files {
                            println!("    {f}");
                        }
                        println!();
                        println!("  Members:");
                        for m in &c.members {
                            println!("    {} ({}) {}", m.name, m.kind, m.file_path);
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                None => {
                    eprintln!("Cluster '{}' not found.", id_or_name);
                    eprintln!(
                        "Available clusters: {}",
                        output
                            .communities
                            .iter()
                            .map(|c| format!("[{}] {}", c.id, c.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    Ok((EXIT_NOT_FOUND, None))
                }
            }
        }

        Commands::Setup {
            tool,
            all,
            allow_writes,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            setup::run_setup(tool.as_deref(), &db_path, all, allow_writes)?;
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Snapshot { command } => run_snapshot(command).map(|c| (c, None)),
        Commands::Instance { command } => run_instance(command).map(|c| (c, None)),
        Commands::Brain { command } => run_brain(*command, out, t0),
        Commands::Embed {
            db,
            endpoint,
            model,
            batch_size,
        } => run_embed(db.as_deref(), &endpoint, &model, batch_size).map(|c| (c, None)),

        Commands::DeadCode {
            min_confidence,
            json,
            db,
        } => {
            let min_conf =
                DeadCodeConfidence::from_str_loose(&min_confidence).unwrap_or_else(|| {
                    eprintln!(
                        "Warning: unknown confidence level '{}', defaulting to 'low'",
                        min_confidence
                    );
                    DeadCodeConfidence::Low
                });
            let store = open_store(db.as_deref())?;
            let result = detect_dead_code(&store)?;

            // Filter by minimum confidence.
            let filtered: Vec<_> = result
                .unreachable_symbols
                .iter()
                .filter(|s| s.confidence >= min_conf)
                .collect();
            let filtered_count = filtered.len();

            if json {
                #[derive(serde::Serialize)]
                struct DeadCodeJson<'a> {
                    total_symbols: usize,
                    reachable_symbols: usize,
                    unreachable_count: usize,
                    dead_percentage: f64,
                    min_confidence: String,
                    unreachable_symbols: Vec<&'a nestweaver_engine::UnreachableSymbol>,
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&DeadCodeJson {
                        total_symbols: result.total_symbols,
                        reachable_symbols: result.reachable_symbols,
                        unreachable_count: filtered_count,
                        dead_percentage: result.dead_percentage,
                        min_confidence: min_conf.to_string(),
                        unreachable_symbols: filtered,
                    })?
                );
            } else if filtered.is_empty() {
                println!(
                    "No dead code detected ({} symbols, all reachable from entry points).",
                    result.total_symbols
                );
            } else {
                println!(
                    "Dead code analysis: {} of {} symbols ({:.1}%) unreachable from entry points\n",
                    result.unreachable_symbols.len(),
                    result.total_symbols,
                    result.dead_percentage,
                );
                if min_conf != DeadCodeConfidence::Low {
                    println!(
                        "Showing {} symbol(s) with confidence >= {}\n",
                        filtered.len(),
                        min_conf
                    );
                }

                // Group by file path.
                let mut by_file: std::collections::BTreeMap<
                    &str,
                    Vec<&nestweaver_engine::UnreachableSymbol>,
                > = std::collections::BTreeMap::new();
                for sym in &filtered {
                    by_file.entry(&sym.file_path).or_default().push(sym);
                }

                for (file, syms) in &by_file {
                    println!("{}:", file);
                    for sym in syms {
                        println!(
                            "  {} ({}) [{}] confidence={}",
                            sym.name, sym.kind, sym.visibility, sym.confidence,
                        );
                    }
                    println!();
                }
            }

            let stats = format!(
                "{} unreachable of {} symbols in {}",
                filtered_count,
                result.total_symbols,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Export {
            format,
            output,
            top,
            db,
        } => {
            let store = open_store(db.as_deref())?;

            let write_to: Box<dyn std::io::Write> = match &output {
                Some(path) => Box::new(
                    std::fs::File::create(path)
                        .with_context(|| format!("failed to create {}", path.display()))?,
                ),
                None => Box::new(std::io::stdout().lock()),
            };
            let mut writer = std::io::BufWriter::new(write_to);

            match format.as_str() {
                "cypher" => export_cypher(&store, &mut writer)?,
                "graphml" => export_graphml(&store, &mut writer)?,
                "mermaid" => export_mermaid(&store, top, &mut writer)?,
                other => {
                    eprintln!(
                        "Unknown format '{}'. Supported: cypher, graphml, mermaid",
                        other
                    );
                    return Ok((EXIT_ERROR, None));
                }
            }

            if let Some(path) = &output {
                out.status(&format!("Exported graph to {}", path.display()));
            }

            let stats = format!("exported {} in {}", format, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "nestweaver", &mut std::io::stdout());
            Ok((EXIT_SUCCESS, None))
        }

        Commands::PrImpact {
            files,
            depth,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;

            // Determine changed files: from --files flag or git diff.
            let changed_files: Vec<PathBuf> = if let Some(files_str) = files {
                files_str
                    .split(',')
                    .map(|s| PathBuf::from(s.trim()))
                    .collect()
            } else {
                let repo_root = detect_repo_root();
                out.status("No --files given, detecting via git diff...");
                changed_files_from_git(&repo_root, None).context("git diff")?
            };

            if changed_files.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "changed_symbols": [],
                            "affected_symbols": [],
                            "affected_clusters": [],
                            "risk_level": "Low",
                            "summary": "No changed files detected.",
                        }))?
                    );
                } else {
                    println!("No changed files detected.");
                }
                return Ok((EXIT_SUCCESS, None));
            }

            out.status(&format!(
                "Analyzing blast radius for {} file(s) (depth={})...",
                changed_files.len(),
                depth
            ));

            let result = analyze_blast_radius(&store, &changed_files, depth, Some(&db_path))?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.summary);
                println!();

                if !result.changed_symbols.is_empty() {
                    println!("Changed symbols ({}):", result.changed_symbols.len());
                    for s in &result.changed_symbols {
                        let pr = s
                            .pagerank_score
                            .map(|p| format!(" pr={p:.4}"))
                            .unwrap_or_default();
                        println!("  {} ({}) {}{pr}", s.name, s.kind, s.file_path);
                    }
                    println!();
                }

                if !result.affected_symbols.is_empty() {
                    println!("Affected symbols ({}):", result.affected_symbols.len());
                    for s in &result.affected_symbols {
                        println!(
                            "  [depth {}] {} via {} ({:.2}) — {}",
                            s.depth, s.name, s.edge_type, s.confidence, s.file_path
                        );
                    }
                    println!();
                }

                if !result.affected_clusters.is_empty() {
                    println!("Affected clusters ({}):", result.affected_clusters.len());
                    for c in &result.affected_clusters {
                        println!(
                            "  [{}] {} — {}/{} members affected (cohesion={:.2})",
                            c.id, c.name, c.affected_count, c.total_count, c.cohesion
                        );
                    }
                    println!();
                }

                println!("Risk level: {:?}", result.risk_level);
            }

            let stats = format!(
                "{} changed, {} affected, risk={:?} in {}",
                result.changed_symbols.len(),
                result.affected_symbols.len(),
                result.risk_level,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Watch { repo, db, instance } => {
            let repo_path = match repo {
                Some(p) => p,
                None => detect_repo_root(),
            };
            if !repo_path.exists() || !repo_path.is_dir() {
                eprintln!(
                    "Error: repo path is not a directory: {}",
                    repo_path.display()
                );
                return Ok((EXIT_ERROR, None));
            }
            let db_path = resolve_index_db_path(db, &repo_path);
            let instance_id = instance.unwrap_or_else(|| "default".to_string());

            let watcher = CodeWatcher::new(&db_path, &repo_path, &instance_id);
            let stop = watcher.shutdown_handle();

            let lock_path = {
                let mut s = db_path.as_os_str().to_owned();
                s.push(".lock");
                PathBuf::from(s)
            };
            let _ = std::fs::write(&lock_path, process::id().to_string());

            let stop_signal = stop.clone();
            let _ = ctrlc_handler(move || stop_signal.stop());

            eprintln!(
                "Watching {} -> {} (Ctrl-C to stop)",
                repo_path.display(),
                db_path.display()
            );
            watcher.run().context("code watcher")?;

            let _ = std::fs::remove_file(&lock_path);
            eprintln!("Watcher stopped.");
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Mcp {
            db,
            allow_mcp_add_sources,
            lite,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            // The server runs until the client closes stdin. Errors bubble
            // up as an EXIT_ERROR; clean EOF exits 0.
            nestweaver_mcp::run_stdio_server(&db_path, allow_mcp_add_sources, lite)
                .context("mcp server")?;
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Ui {
            db,
            port,
            config: _config,
            no_open,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            let state = nestweaver_web::state::AppState::new(store, tantivy, db_path);

            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(nestweaver_web::start_server(state, port, !no_open))?;

            Ok((EXIT_SUCCESS, None))
        }

        Commands::Search {
            query,
            limit,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;
            let candidates = search_symbols(&store, &query, limit)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&candidates)?);
            } else if candidates.is_empty() {
                println!("No symbols found matching '{query}'.");
            } else {
                println!("Found {} symbol(s) matching '{query}':", candidates.len());
                for c in &candidates {
                    println!("  {} ({}) {}:{}", c.name, c.kind, c.file_path, c.start_line);
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Symbol {
            name_or_uid,
            json,
            db,
            ..
        } => {
            let store = open_store(db.as_deref())?;
            let result = lookup_symbol(&store, &name_or_uid)?;

            match result {
                LookupResult::Found(detail) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&*detail)?);
                    } else {
                        let s = &detail.symbol;
                        if out.verbose {
                            println!("Symbol: {} [{}]", s.name, s.uid);
                        } else {
                            println!("Symbol: {}", s.name);
                        }
                        println!("Kind: {}", s.kind);
                        println!("File: {}:{}", s.file_path, s.start_line);
                        println!("Signature: {}", s.signature);

                        if !detail.callers.is_empty() {
                            if !out.quiet {
                                println!("\nCallers ({}):", detail.callers.len());
                            }
                            for c in &detail.callers {
                                if out.verbose {
                                    println!(
                                        "  {} ({}:{}) [{}]",
                                        c.name, c.file_path, c.start_line, c.uid
                                    );
                                } else {
                                    println!("  {} ({}:{})", c.name, c.file_path, c.start_line);
                                }
                            }
                        }

                        if !detail.callees.is_empty() {
                            if !out.quiet {
                                println!("\nCallees ({}):", detail.callees.len());
                            }
                            for c in &detail.callees {
                                if out.verbose {
                                    println!(
                                        "  {} ({}:{}) [{}]",
                                        c.name, c.file_path, c.start_line, c.uid
                                    );
                                } else {
                                    println!("  {} ({}:{})", c.name, c.file_path, c.start_line);
                                }
                            }
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                LookupResult::NotFound => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"error": "not found", "name": name_or_uid})
                        );
                    } else {
                        eprintln!("Symbol '{name_or_uid}' not found.");
                    }
                    Ok((EXIT_NOT_FOUND, None))
                }
                LookupResult::Ambiguous(candidates) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&candidates)?);
                    } else {
                        eprintln!(
                            "Ambiguous: '{}' matches {} symbols:",
                            name_or_uid,
                            candidates.len()
                        );
                        for c in &candidates {
                            eprintln!("  {} [{}] {}:{}", c.uid, c.kind, c.file_path, c.start_line);
                        }
                    }
                    Ok((EXIT_AMBIGUOUS, None))
                }
            }
        }

        Commands::Impact {
            name_or_uid,
            depth,
            confidence,
            json,
            db,
            ..
        } => {
            let store = open_store(db.as_deref())?;

            // Resolve the symbol UID first (may be a name).
            match resolve_uid(&store, &name_or_uid)? {
                ResolveResult::Found(uid) => {
                    let nodes = store.impact(&uid, depth, confidence)?;
                    let count = nodes.len();

                    if json {
                        // Serialize as JSON array
                        #[derive(serde::Serialize)]
                        struct ImpactNodeJson {
                            uid: String,
                            name: String,
                            file_path: String,
                            start_line: u32,
                            edge_type: String,
                            confidence: f32,
                            depth: u32,
                        }
                        let json_nodes: Vec<_> = nodes
                            .iter()
                            .map(|n| ImpactNodeJson {
                                uid: n.uid.clone(),
                                name: n.name.clone(),
                                file_path: n.file_path.clone(),
                                start_line: n.start_line,
                                edge_type: n.edge_type.clone(),
                                confidence: n.confidence,
                                depth: n.depth,
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json_nodes)?);
                    } else if nodes.is_empty() {
                        if !out.quiet {
                            println!("No impact found for '{name_or_uid}'.");
                        }
                    } else {
                        if !out.quiet {
                            println!("Impact of '{name_or_uid}' ({} nodes):", count);
                        }
                        for n in &nodes {
                            if out.verbose {
                                println!(
                                    "  [depth {}] {} via {} ({:.2}) — {}:{} [{}]",
                                    n.depth,
                                    n.name,
                                    n.edge_type,
                                    n.confidence,
                                    n.file_path,
                                    n.start_line,
                                    n.uid,
                                );
                            } else {
                                println!(
                                    "  [depth {}] {} via {} ({:.2}) — {}:{}",
                                    n.depth,
                                    n.name,
                                    n.edge_type,
                                    n.confidence,
                                    n.file_path,
                                    n.start_line,
                                );
                            }
                        }
                    }
                    let stats = format!(
                        "{} affected symbols in {}",
                        count,
                        format_elapsed(t0.elapsed())
                    );
                    Ok((EXIT_SUCCESS, Some(stats)))
                }
                ResolveResult::NotFound => {
                    eprintln!("Symbol '{name_or_uid}' not found.");
                    Ok((EXIT_NOT_FOUND, None))
                }
                ResolveResult::Ambiguous(candidates) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&candidates)?);
                    } else {
                        eprintln!(
                            "Ambiguous: '{}' matches {} symbols:",
                            name_or_uid,
                            candidates.len()
                        );
                        for c in &candidates {
                            eprintln!("  {} [{}] {}:{}", c.uid, c.kind, c.file_path, c.start_line);
                        }
                    }
                    Ok((EXIT_AMBIGUOUS, None))
                }
            }
        }

        Commands::ListProjects { json, db } => {
            let store = open_store(db.as_deref())?;
            let projects = store.list_projects().map_err(|e| anyhow::anyhow!(e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&projects)?);
            } else if projects.is_empty() {
                println!(
                    "No projects found. Use an instance config with [[projects]] to define them."
                );
            } else {
                for p in &projects {
                    println!("{}", p.name);
                    println!("  UID:      {}", p.uid);
                    println!("  Instance: {}", p.instance_id);
                    if let Some(ref summary) = p.summary {
                        println!("  Summary:  {summary}");
                    }
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ProjectContext {
            name,
            token_budget,
            include_components,
            json,
            db,
            since,
            recency_weight,
            recency_half_life_days,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // Resolve the project: name -> alias -> UID substring.
            let project = if name.starts_with("proj:") {
                let all = store.list_projects().map_err(|e| anyhow::anyhow!(e))?;
                all.into_iter()
                    .find(|p| p.uid == name || p.uid.contains(&name))
                    .ok_or_else(|| anyhow::anyhow!("project '{}' not found", name))?
            } else {
                match store
                    .lookup_project_by_name(&name)
                    .map_err(|e| anyhow::anyhow!(e))?
                {
                    Some(p) => p,
                    None => {
                        // Try alias match via extension sidecar, then UID substring.
                        let all = store.list_projects().map_err(|e| anyhow::anyhow!(e))?;
                        let ext_store = load_extensions(&db_path);
                        let needle = name.to_lowercase();
                        let alias_match = all.iter().find(|p| {
                            if let Some(serde_json::Value::Array(aliases)) =
                                ext_store.get(&p.uid).and_then(|m| m.get("aliases"))
                            {
                                aliases
                                    .iter()
                                    .any(|a| a.as_str().is_some_and(|s| s.to_lowercase() == needle))
                            } else {
                                false
                            }
                        });
                        if let Some(p) = alias_match {
                            p.clone()
                        } else {
                            match all.into_iter().find(|p| p.uid.contains(&name)) {
                                Some(p) => p,
                                None => {
                                    eprintln!(
                                        "Project '{}' not found. Try: nestweaver list-projects",
                                        name
                                    );
                                    return Ok((EXIT_NOT_FOUND, None));
                                }
                            }
                        }
                    }
                }
            };

            // Collect seed UIDs from this project (and optionally its components).
            let mut seed_uids: Vec<String> = Vec::new();
            let note_uids = store
                .list_project_note_uids(&project.uid)
                .map_err(|e| anyhow::anyhow!(e))?;
            seed_uids.extend(note_uids);
            let sym_uids = store
                .list_project_symbol_uids(&project.uid)
                .map_err(|e| anyhow::anyhow!(e))?;
            seed_uids.extend(sym_uids);

            if include_components {
                let comp_uids = store
                    .list_project_component_uids(&project.uid)
                    .map_err(|e| anyhow::anyhow!(e))?;
                for comp_uid in &comp_uids {
                    seed_uids.extend(store.list_project_note_uids(comp_uid).unwrap_or_default());
                    seed_uids.extend(store.list_project_symbol_uids(comp_uid).unwrap_or_default());
                }
            }

            // Deduplicate seeds.
            let mut seen = std::collections::HashSet::new();
            seed_uids.retain(|u| seen.insert(u.clone()));

            if seed_uids.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "project": project.name,
                            "seeds": [],
                            "connected": [],
                            "note": "No notes or symbols associated with this project.",
                        }))?
                    );
                } else {
                    println!(
                        "Project '{}' has no associated notes or symbols.",
                        project.name
                    );
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let defaults = HybridSearchConfig::default();
            let aliases = load_alias_sidecar(&db_path);
            match build_brain_context_hybrid_with_aliases(
                &store,
                &seed_uids,
                tantivy.as_ref(),
                &defaults,
                &aliases,
                Some(&db_path),
                Some(nestweaver_store::QueryIntent::ProjectContext),
            ) {
                Ok(mut result) => {
                    // Post-PPR scope boost: multiply relevance for nodes that
                    // belong to the project so declared content ranks highest.
                    let seed_set_boost: std::collections::HashSet<&str> =
                        seed_uids.iter().map(|s| s.as_str()).collect();
                    for node in &mut result.connected {
                        if seed_set_boost.contains(node.uid.as_str()) {
                            node.relevance *= 5.0;
                        }
                    }
                    result.connected.sort_by(|a, b| {
                        b.relevance
                            .partial_cmp(&a.relevance)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    // since filter: hard filter Note/Section nodes by modified_at.
                    if let Some(ref since_ts) = since {
                        let recent_notes = store
                            .list_note_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let recent_sections = store
                            .list_section_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_since = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                if item.kind.to_lowercase().contains("symbol") {
                                    return true;
                                }
                                recent_notes.contains(&item.uid)
                                    || recent_sections.contains(&item.uid)
                            });
                        };
                        filter_since(&mut result.seeds);
                        filter_since(&mut result.connected);
                    }

                    // recency bias: soft boost based on note modified_at age.
                    if recency_weight > 0.0 {
                        apply_recency_bias_cli(
                            &store,
                            &mut result.connected,
                            recency_weight,
                            recency_half_life_days,
                        );
                        apply_recency_bias_cli(
                            &store,
                            &mut result.seeds,
                            recency_weight,
                            recency_half_life_days,
                        );
                    }

                    let cut = token_budgeted_truncate(&result.connected, token_budget);
                    if json {
                        print_brain_context_json(&result, cut)?;
                    } else {
                        println!("Project: {}  ({})", project.name, project.uid);
                        if let Some(ref summary) = project.summary {
                            println!("  {summary}");
                        }
                        println!();
                        print_brain_context_text(&result, cut, Some(token_budget));
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    Ok((EXIT_ERROR, None))
                }
            }
        }

        Commands::Index {
            repo,
            instance,
            db,
            force,
        } => {
            let repo_path = match repo {
                Some(p) => p,
                None => detect_repo_root(),
            };
            let db_path = resolve_index_db_path(db, &repo_path);
            let instance_id = instance.as_deref().unwrap_or("default");

            let repo_url = format!("file://{}", repo_path.display());

            out.status(&format!("Indexing {}", repo_path.display()));

            let (files_count, symbols_count, edges_count);

            if force {
                // Full re-index requested explicitly.
                let result = index_directory(&repo_path, &db_path, instance_id, &repo_url, "local")
                    .context("index_directory")?;

                files_count = result.files_count;
                symbols_count = result.symbols_count;
                edges_count = result.edges_count;

                println!(
                    "Indexed {} file(s), {} symbol(s), {} edge(s).",
                    files_count, symbols_count, edges_count
                );

                if !result.skipped_files.is_empty() {
                    out.status(&format!("Skipped {} file(s):", result.skipped_files.len()));
                    for sf in &result.skipped_files {
                        out.status(&format!("  {} — {}", sf.path, sf.reason));
                    }
                }
            } else {
                // Incremental index (falls back to full when no prior index exists).
                let inc = incremental_index(&repo_path, &db_path, instance_id, &repo_url)
                    .context("incremental_index")?;

                files_count = inc.files_added + inc.files_modified;
                symbols_count = inc.symbols_added;
                edges_count = 0; // not tracked separately in incremental

                if inc.fell_back_to_full {
                    out.status("Incremental: no prior index found, performed full index.");
                } else {
                    out.status(&format!(
                        "Incremental: {} added, {} modified, {} deleted, {} renamed, {} skipped.",
                        inc.files_added,
                        inc.files_modified,
                        inc.files_deleted,
                        inc.files_renamed,
                        inc.files_skipped,
                    ));
                    out.status(&format!(
                        "Incremental: {} symbol(s) added, {} symbol(s) removed.",
                        inc.symbols_added, inc.symbols_removed,
                    ));
                }
            }

            // Compute PageRank after indexing so the repo-map is immediately usable.
            // (incremental_index already computes PageRank internally, but recomputing
            // here is cheap and ensures the sidecar is always up to date for the full path.)
            out.status("Computing PageRank...");
            let store = GraphStore::open(&db_path)
                .with_context(|| format!("failed to open database at {}", db_path.display()))?;
            store
                .compute_pagerank(0.85, 20, &GraphScope::code_only())
                .with_context(|| "compute_pagerank")?;

            // Save PageRank cache alongside the DB for use by subsequent commands.
            let pr_path = db_path.with_extension("pagerank.json");
            store
                .save_pagerank_cache(&pr_path)
                .with_context(|| "save_pagerank_cache")?;
            out.status("PageRank complete.");

            let stats = format!(
                "{} files, {} symbols, {} edges in {}",
                files_count,
                symbols_count,
                edges_count,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }
    }
}

enum ResolveResult {
    Found(String),
    NotFound,
    Ambiguous(Vec<Symbol>),
}

/// Resolve a name-or-uid to a UID.
/// Returns `Ambiguous` when the name matches multiple symbols (callers should report and exit 3).
fn resolve_uid(store: &GraphStore, name_or_uid: &str) -> anyhow::Result<ResolveResult> {
    if name_or_uid.contains(':') {
        // Treat as UID directly.
        match store.lookup_symbol(name_or_uid) {
            Ok(sym) => Ok(ResolveResult::Found(sym.uid)),
            Err(nestweaver_store::StoreError::NotFound) => Ok(ResolveResult::NotFound),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    } else {
        let mut matches = store
            .lookup_symbols_by_name(name_or_uid)
            .map_err(|e| anyhow::anyhow!(e))?;
        match matches.len() {
            0 => Ok(ResolveResult::NotFound),
            1 => Ok(ResolveResult::Found(matches.remove(0).uid)),
            _ => Ok(ResolveResult::Ambiguous(matches)),
        }
    }
}

/// Print a human-readable representation of a `ContextResult`.
fn print_context_text(result: &ContextResult) {
    println!("Seeds ({} resolved):", result.seeds.len());
    for node in &result.seeds {
        println!(
            "  {}  {}  {}:{}",
            node.name, node.kind, node.file_path, node.start_line
        );
    }

    if !result.connected.is_empty() {
        println!();
        println!(
            "Connected ({} symbols, ranked by relevance):",
            result.connected.len()
        );
        for node in &result.connected {
            println!(
                "  {}  {}  {}:{}  {:.4}",
                node.name, node.kind, node.file_path, node.start_line, node.relevance
            );
        }
    }

    if !result.cross_repo_links.is_empty() {
        println!();
        println!("Cross-repo links:");
        for link in &result.cross_repo_links {
            println!(
                "  {}  {}  {:.2}",
                link.package, link.link_type, link.confidence
            );
        }
    }
}

/// Print a human-readable representation of a `FeatureContextResult`.
fn print_feature_context_text(result: &FeatureContextResult) {
    println!("Feature: {}", result.feature.name);
    if let Some(desc) = &result.feature.description {
        println!("  {desc}");
    }
    println!("  Repos: {}", result.feature.repos.join(", "));

    if !result.links.is_empty() {
        println!();
        println!("Links:");
        for link in &result.links {
            let desc = link
                .description
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            println!("  {} → {} ({}){desc}", link.from, link.to, link.link_type);
        }
    }

    println!();
    println!("Seeds ({} resolved):", result.seeds.len());
    for node in &result.seeds {
        println!(
            "  {}  {}  {}:{}",
            node.name, node.kind, node.file_path, node.start_line
        );
    }

    if !result.connected.is_empty() {
        println!();
        println!(
            "Connected ({} symbols, ranked by relevance):",
            result.connected.len()
        );
        for node in &result.connected {
            println!(
                "  {}  {}  {}:{}  {:.4}",
                node.name, node.kind, node.file_path, node.start_line, node.relevance
            );
        }
    }
}

/// Register a handler for SIGINT (Ctrl-C) and SIGTERM so watcher loops
/// can shut down gracefully, flush the WAL, and remove lock files.
///
/// The callback `on_signal` is invoked from a signal-safe context; it
/// should only flip an `AtomicBool` (as `ShutdownHandle::stop` does).
fn ctrlc_handler<F>(mut on_signal: F) -> Result<(), ()>
where
    F: FnMut() + Send + 'static,
{
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fired = Arc::new(AtomicBool::new(false));

    // Register for both SIGINT and SIGTERM. The flag prevents calling
    // the closure more than once (idempotent, but saves work).
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let fired = fired.clone();
        // SAFETY: `signal_hook::flag::register` is signal-safe; it sets
        // the AtomicBool from the signal handler context.
        if signal_hook::flag::register(sig, fired.clone()).is_err() {
            tracing::warn!("failed to register signal handler for {sig}");
        }
    }

    // Spawn a background thread that polls the flag. When set, call the
    // user-provided closure (which flips the watcher's shutdown handle).
    std::thread::Builder::new()
        .name("signal-handler".into())
        .spawn(move || {
            while !fired.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            on_signal();
        })
        .map_err(|_| ())?;

    Ok(())
}

/// Dispatch a `brain` subcommand.
fn run_brain(
    command: BrainCommands,
    out: &OutputConfig,
    t0: std::time::Instant,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        BrainCommands::Add {
            path,
            name,
            instance,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let instance_id = instance.as_deref().unwrap_or("default");
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });

            if !path.exists() {
                eprintln!("Error: path does not exist: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            if !path.is_dir() {
                eprintln!("Error: path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }

            // Auto-detect: report what we think the source is.
            let has_obsidian = path.join(".obsidian").is_dir();
            let kind_hint = if has_obsidian {
                "Obsidian vault"
            } else {
                "markdown folder"
            };
            out.status(&format!(
                "Detected {} at {} -> {}",
                kind_hint,
                path.display(),
                db_path.display()
            ));

            let result = index_markdown_directory(&path, &db_path, instance_id, &vault_name)
                .context("index_markdown_directory")?;

            // Record the indexer run timestamp for this vault.
            if let Err(e) = record_last_indexed_at(&db_path, &result.vault_uid) {
                tracing::warn!("failed to record last_indexed_at: {e}");
            }

            let notes_count = result.notes_count;

            if result.notes_count == 0 {
                // The Vault node was created, but no markdown files were
                // found. Tell the user clearly rather than print a row of
                // zeros that looks like an indexing bug.
                println!(
                    "No markdown files found in {}. Vault '{}' was registered \
                     so the watcher can pick up notes added later.",
                    path.display(),
                    result.vault_name,
                );
            } else {
                println!(
                    "Indexed vault '{}': {} note(s), {} heading(s), {} section(s), \
                     {} tag(s), {} wikilink(s) ({} unresolved).",
                    result.vault_name,
                    result.notes_count,
                    result.headings_count,
                    result.sections_count,
                    result.tags_count,
                    result.wikilinks_resolved,
                    result.wikilinks_unresolved,
                );
            }

            // Auto-discover cross-domain (notes ↔ code) bridges if any
            // code symbols are indexed. Cheap no-op when there's no code.
            {
                let store_for_discovery = open_store(Some(&db_path))?;
                match discover_cross_domain_links(&store_for_discovery) {
                    Ok(cd) if cd.note_to_symbol_edges + cd.section_to_symbol_edges > 0 => {
                        println!(
                            "Cross-domain: {} note→symbol, {} section→symbol edge(s) created.",
                            cd.note_to_symbol_edges, cd.section_to_symbol_edges
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("cross-domain discovery failed: {e}"),
                }
            }

            if !result.skipped.is_empty() {
                out.status(&format!("Skipped {} file(s):", result.skipped.len()));
                for sf in &result.skipped {
                    out.status(&format!("  {} - {}", sf.path, sf.reason));
                }
            }

            let stats = format!("{} notes in {}", notes_count, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::List { json, db } => {
            let db_default = default_db_path();
            let db_path = db.as_deref().unwrap_or(&db_default);
            let store = open_store(Some(db_path))?;
            let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;

            // Compute (note_count, last_indexed) per vault. Prefer the
            // extension-store timestamp (actual indexer run) with fallback
            // to max(note.modified_at).
            let per_vault: Vec<(String, usize, Option<String>)> = vaults
                .iter()
                .map(|v| {
                    let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
                    let last = get_last_indexed_at(db_path, &v.uid)
                        .or_else(|| notes.iter().filter_map(|n| n.modified_at.clone()).max());
                    (v.uid.clone(), notes.len(), last)
                })
                .collect();

            if json {
                #[derive(serde::Serialize)]
                struct VaultRow {
                    uid: String,
                    name: String,
                    root_path: String,
                    notes: usize,
                    last_indexed: Option<String>,
                }
                let rows: Vec<VaultRow> = vaults
                    .iter()
                    .zip(per_vault.iter())
                    .map(|(v, (_, notes, last))| VaultRow {
                        uid: v.uid.clone(),
                        name: v.name.clone(),
                        root_path: v.root_path.clone(),
                        notes: *notes,
                        last_indexed: last.clone(),
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if vaults.is_empty() {
                println!("No vaults indexed. Try: nestweaver brain add <path>");
            } else {
                for (v, (_, notes, last)) in vaults.iter().zip(per_vault.iter()) {
                    println!("{}", v.name);
                    println!("  UID:   {}", v.uid);
                    println!("  Path:  {}", v.root_path);
                    println!("  Notes: {notes}");
                    println!("  Last indexed: {}", last.as_deref().unwrap_or("(unknown)"));
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Status { json, db } => {
            let db_default = default_db_path();
            let db_path = db.as_deref().unwrap_or(&db_default);
            let store = open_store(Some(db_path))?;
            let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;
            let note_count = store.count_notes().map_err(|e| anyhow::anyhow!(e))?;
            let heading_count = store.count_headings().map_err(|e| anyhow::anyhow!(e))?;
            let section_count = store.count_sections().map_err(|e| anyhow::anyhow!(e))?;
            let tag_count = store.count_tags().map_err(|e| anyhow::anyhow!(e))?;
            let wikilink_count = store
                .count_wikilink_edges()
                .map_err(|e| anyhow::anyhow!(e))?;
            let repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;

            /// Resolve last_indexed_at for a vault: prefer the extension-store
            /// timestamp (actual indexer run), fall back to max(note.modified_at)
            /// for databases indexed before this feature was added.
            fn resolve_last_indexed(
                db_path: &Path,
                vault_uid: &str,
                store: &GraphStore,
            ) -> Option<String> {
                if let Some(ts) = get_last_indexed_at(db_path, vault_uid) {
                    return Some(ts);
                }
                // Fallback: max(note.modified_at).
                store
                    .list_notes(Some(vault_uid))
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|n| n.modified_at.clone())
                    .max()
            }

            if json {
                #[derive(serde::Serialize)]
                struct VaultDetail {
                    name: String,
                    root_path: String,
                    note_count: usize,
                    last_indexed: Option<String>,
                }
                #[derive(serde::Serialize)]
                struct Status {
                    db: String,
                    vaults: usize,
                    vault_details: Vec<VaultDetail>,
                    notes: usize,
                    headings: usize,
                    sections: usize,
                    tags: usize,
                    wikilinks: usize,
                    repos: usize,
                    instance_ids: Vec<String>,
                }
                let vault_details: Vec<VaultDetail> = vaults
                    .iter()
                    .map(|v| {
                        let vault_note_count =
                            store.list_notes(Some(&v.uid)).unwrap_or_default().len();
                        let last_indexed = resolve_last_indexed(db_path, &v.uid, &store);
                        VaultDetail {
                            name: v.name.clone(),
                            root_path: v.root_path.clone(),
                            note_count: vault_note_count,
                            last_indexed,
                        }
                    })
                    .collect();
                let mut instance_ids: std::collections::BTreeSet<&str> =
                    vaults.iter().map(|v| v.instance_id.as_str()).collect();
                instance_ids.extend(repos.iter().map(|r| r.instance_id.as_str()));
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Status {
                        db: db_path.display().to_string(),
                        vaults: vaults.len(),
                        vault_details,
                        notes: note_count,
                        headings: heading_count,
                        sections: section_count,
                        tags: tag_count,
                        wikilinks: wikilink_count,
                        repos: repos.len(),
                        instance_ids: instance_ids.into_iter().map(|s| s.to_string()).collect(),
                    })?
                );
            } else {
                println!("Brain status:");
                println!("  Database:  {}", db_path.display());
                println!("  Vaults:    {}", vaults.len());
                for v in &vaults {
                    let vault_note_count = store.list_notes(Some(&v.uid)).unwrap_or_default().len();
                    let last_indexed = resolve_last_indexed(db_path, &v.uid, &store)
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "    - {} ({vault_note_count} notes, last indexed: {last_indexed})",
                        v.name
                    );
                }
                println!("  Notes:     {note_count}");
                println!("  Headings:  {heading_count}");
                println!("  Sections:  {section_count}");
                println!("  Tags:      {tag_count}");
                println!("  Wikilinks: {wikilink_count}");
                println!("  Repos:     {}", repos.len());
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Watch {
            path,
            name,
            instance,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            if !path.exists() || !path.is_dir() {
                eprintln!("Error: vault path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
            let instance_id = instance.unwrap_or_else(|| "default".to_string());

            let tantivy_sidecar = tantivy_sidecar_path_for(&db_path);
            let manifests_path = db_path.with_extension("manifests.json");
            let watcher = BrainWatcher::new(&db_path, &path, instance_id, vault_name)
                .with_tantivy_index(&tantivy_sidecar)
                .with_manifests_path(&manifests_path);
            let stop = watcher.shutdown_handle();

            // Write a PID lock file so MCP servers and other readers know a
            // watcher is active and should open the database read-only.
            let lock_path = {
                let mut s = db_path.as_os_str().to_owned();
                s.push(".lock");
                std::path::PathBuf::from(s)
            };
            let _ = std::fs::write(&lock_path, std::process::id().to_string());

            // Wire Ctrl-C → shutdown_handle.stop(). Best-effort; if the
            // signal handler can't install we still run, the user just
            // has to kill the process.
            let stop_signal = stop.clone();
            let _ = ctrlc_handler(move || stop_signal.stop());

            out.status(&format!(
                "Watching {} -> {} (Ctrl-C to stop)",
                path.display(),
                db_path.display()
            ));
            watcher.run().context("watcher")?;

            // Clean up the lock file on orderly shutdown.
            let _ = std::fs::remove_file(&lock_path);
            out.status("Watcher stopped.");
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Refresh {
            path,
            name,
            instance,
            db,
            since,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            if !path.exists() || !path.is_dir() {
                eprintln!("Error: vault path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
            let instance_id = instance.unwrap_or_else(|| "default".to_string());

            // Compute vault UID for recording last_indexed_at.
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let v_uid = nestweaver_schema::vault_uid(&instance_id, &canonical.to_string_lossy());

            if let Some(since_str) = since {
                // Incremental refresh: only re-index files modified since the
                // given timestamp.
                let since_time = parse_iso8601_to_system_time(&since_str).with_context(|| {
                    format!(
                        "invalid --since timestamp '{}': expected ISO 8601 (e.g. 2026-05-26T00:00:00Z)",
                        since_str
                    )
                })?;
                let result = index_markdown_directory_since(
                    &path,
                    &db_path,
                    &instance_id,
                    &vault_name,
                    since_time,
                )
                .context("index_markdown_directory_since")?;

                // Record the indexer run timestamp.
                if let Err(e) = record_last_indexed_at(&db_path, &v_uid) {
                    tracing::warn!("failed to record last_indexed_at: {e}");
                }

                println!(
                    "Incremental refresh of vault '{}' (since {}): \
                     checked {} file(s), updated {} note(s), \
                     {} heading(s), {} section(s), {} tag(s), {} wikilink(s).",
                    result.vault_name,
                    since_str,
                    result.files_checked,
                    result.notes_updated,
                    result.headings_count,
                    result.sections_count,
                    result.tags_count,
                    result.wikilinks_resolved,
                );
            } else {
                // Full refresh: cascade-delete all notes then re-index from scratch.
                let store = open_store(Some(&db_path))?;
                let existing = store.list_notes(Some(&v_uid)).unwrap_or_default();
                let drop_count = existing.len();
                for n in &existing {
                    if let Err(e) = store.delete_note_cascade(&n.uid) {
                        tracing::warn!("delete_note_cascade {} failed: {e}", n.uid);
                    }
                }
                drop(store);

                let result = index_markdown_directory(&path, &db_path, &instance_id, &vault_name)
                    .context("index_markdown_directory")?;

                // Record the indexer run timestamp.
                if let Err(e) = record_last_indexed_at(&db_path, &v_uid) {
                    tracing::warn!("failed to record last_indexed_at: {e}");
                }

                println!(
                    "Refreshed vault '{}': dropped {} stale note(s), reindexed {} note(s), \
                     {} heading(s), {} section(s), {} tag(s), {} wikilink(s) ({} unresolved).",
                    result.vault_name,
                    drop_count,
                    result.notes_count,
                    result.headings_count,
                    result.sections_count,
                    result.tags_count,
                    result.wikilinks_resolved,
                    result.wikilinks_unresolved,
                );
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Remove { path, instance, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let instance_id = instance.unwrap_or_else(|| "default".to_string());

            // Resolve the vault UID the same way the indexer / watcher do.
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let v_uid = nestweaver_schema::vault_uid(&instance_id, &canonical.to_string_lossy());

            let store = open_store(Some(&db_path))?;
            // lookup_vault gives us a friendly name for the success line.
            let vault_name = store
                .lookup_vault(&v_uid)
                .map(|v| v.name)
                .unwrap_or_else(|_| {
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("vault")
                        .to_string()
                });
            let dropped = store
                .delete_vault_cascade(&v_uid)
                .context("delete_vault_cascade")?;
            println!(
                "Removed vault '{}' ({} note(s) dropped). Tantivy + PPR sidecars \
                 may be stale; run `nestweaver brain reindex-search` if you want \
                 to clear them too.",
                vault_name, dropped
            );
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::ReindexSearch { db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let sidecar = tantivy_sidecar_path_for(&db_path);
            let store = open_store(Some(&db_path))?;
            let idx = TantivyIndex::open_or_create(&sidecar)
                .with_context(|| format!("open tantivy at {}", sidecar.display()))?;
            let count = idx
                .reindex_from_store(&store)
                .with_context(|| "reindex Tantivy from store")?;
            println!(
                "Tantivy reindex complete: {count} document(s) at {}",
                sidecar.display()
            );
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Search {
            query,
            limit,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            let result_count;
            if let Some(ref idx) = tantivy {
                let hits = idx
                    .search(&query, limit)
                    .with_context(|| "tantivy search")?;
                result_count = hits.len();
                if json {
                    let results: Vec<serde_json::Value> = hits
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "uid": h.uid,
                                "kind": h.kind,
                                "title": h.title,
                                "score": h.score,
                                "vault_uid": h.vault_uid,
                            })
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "query": query,
                            "engine": "bm25",
                            "results": results,
                            "total_matches": hits.len(),
                        }))?
                    );
                } else if hits.is_empty() {
                    println!("No results for '{query}'.");
                } else {
                    println!("Brain search (BM25): {} result(s)\n", hits.len());
                    for h in &hits {
                        println!("  [{:.2}] {} ({})", h.score, h.title, h.kind);
                    }
                }
            } else {
                let needle = query.to_lowercase();
                let notes = store.list_notes(None).context("list_notes")?;
                let matches: Vec<_> = notes
                    .iter()
                    .filter(|n| n.title.to_lowercase().contains(&needle))
                    .take(limit)
                    .collect();
                result_count = matches.len();
                if json {
                    let results: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|n| {
                            serde_json::json!({
                                "uid": n.uid,
                                "kind": "note",
                                "title": n.title,
                                "path": n.file_path,
                                "word_count": n.word_count,
                            })
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "query": query,
                            "engine": "substring",
                            "results": results,
                            "total_matches": matches.len(),
                        }))?
                    );
                } else if matches.is_empty() {
                    println!("No results for '{query}'.");
                } else {
                    println!(
                        "Brain search (substring fallback): {} result(s)\n",
                        matches.len()
                    );
                    for n in &matches {
                        println!("  {} @ {}", n.title, n.file_path);
                    }
                }
            }
            let stats = format!(
                "{} results in {}",
                result_count,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::Context {
            seeds,
            token_budget,
            limit,
            json,
            db,
            kinds,
            repos,
            vaults,
            path_prefix,
            tags,
            exclude_tags,
            weight_ppr,
            weight_bm25,
            weight_semantic,
            since,
            recency_weight,
            recency_half_life_days,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // RFC #6: build custom HybridSearchConfig from optional CLI flags.
            let defaults = HybridSearchConfig::default();
            let config = HybridSearchConfig {
                weight_ppr: weight_ppr.unwrap_or(defaults.weight_ppr),
                weight_bm25: weight_bm25.unwrap_or(defaults.weight_bm25),
                weight_semantic: weight_semantic.unwrap_or(defaults.weight_semantic),
                ..defaults
            };

            let aliases = load_alias_sidecar(&db_path);
            match build_brain_context_hybrid_with_aliases(
                &store,
                &seeds,
                tantivy.as_ref(),
                &config,
                &aliases,
                Some(&db_path),
                None,
            ) {
                Ok(mut result) => {
                    // RFC #2: apply post-PPR filters when any filter flag was set.
                    let filter_kinds_lower: Vec<String> =
                        kinds.iter().map(|k| k.to_lowercase()).collect();
                    let apply_filters = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                        if !filter_kinds_lower.is_empty() {
                            nodes.retain(|n| {
                                let kind_lower = n.kind.to_lowercase();
                                filter_kinds_lower
                                    .iter()
                                    .any(|k| kind_lower.starts_with(k.as_str()))
                            });
                        }
                        if !repos.is_empty() {
                            nodes.retain(|n| {
                                repos.iter().any(|r| {
                                    n.uid.contains(r.as_str()) || n.location.contains(r.as_str())
                                })
                            });
                        }
                        if !vaults.is_empty() {
                            nodes.retain(|n| {
                                vaults.iter().any(|v| {
                                    n.uid.contains(v.as_str()) || n.location.contains(v.as_str())
                                })
                            });
                        }
                        if let Some(ref prefix) = path_prefix {
                            nodes.retain(|n| n.location.starts_with(prefix.as_str()));
                        }
                    };
                    apply_filters(&mut result.seeds);
                    apply_filters(&mut result.connected);

                    // tags filter: keep only note/section nodes tagged with any of these.
                    if !tags.is_empty() {
                        let tagged_notes = store
                            .list_note_uids_with_tags(&tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let tagged_sections = store
                            .list_section_uids_with_tags(&tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_tagged = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                if item.kind.to_lowercase().contains("symbol") {
                                    return true;
                                }
                                tagged_notes.contains(&item.uid)
                                    || tagged_sections.contains(&item.uid)
                            });
                        };
                        filter_tagged(&mut result.seeds);
                        filter_tagged(&mut result.connected);
                    }

                    // exclude_tags filter: remove note/section nodes tagged with any of these.
                    if !exclude_tags.is_empty() {
                        let excluded_notes = store
                            .list_note_uids_with_tags(&exclude_tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let excluded_sections = store
                            .list_section_uids_with_tags(&exclude_tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_excluded = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                !excluded_notes.contains(&item.uid)
                                    && !excluded_sections.contains(&item.uid)
                            });
                        };
                        filter_excluded(&mut result.seeds);
                        filter_excluded(&mut result.connected);
                    }

                    // since filter: hard filter Note/Section nodes by modified_at.
                    if let Some(ref since_ts) = since {
                        let recent_notes = store
                            .list_note_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let recent_sections = store
                            .list_section_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_since = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                if item.kind.to_lowercase().contains("symbol") {
                                    return true;
                                }
                                recent_notes.contains(&item.uid)
                                    || recent_sections.contains(&item.uid)
                            });
                        };
                        filter_since(&mut result.seeds);
                        filter_since(&mut result.connected);
                    }

                    // recency bias: soft boost based on note modified_at age.
                    if recency_weight > 0.0 {
                        apply_recency_bias_cli(
                            &store,
                            &mut result.connected,
                            recency_weight,
                            recency_half_life_days,
                        );
                        apply_recency_bias_cli(
                            &store,
                            &mut result.seeds,
                            recency_weight,
                            recency_half_life_days,
                        );
                    }

                    // token_budget takes precedence over the count-based limit.
                    let cut = match token_budget {
                        Some(budget) => token_budgeted_truncate(&result.connected, budget),
                        None => limit.min(result.connected.len()),
                    };
                    let node_count = result.seeds.len() + cut;
                    if json {
                        print_brain_context_json(&result, cut)?;
                    } else {
                        print_brain_context_text(&result, cut, token_budget);
                    }
                    let stats = format!("{} nodes in {}", node_count, format_elapsed(t0.elapsed()));
                    Ok((EXIT_SUCCESS, Some(stats)))
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("No seeds resolved") {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "seeds_expanded": 0,
                                    "connected": [],
                                    "unresolved_seeds": seeds,
                                }))?
                            );
                        } else {
                            eprintln!("{msg}");
                        }
                        Ok((EXIT_NOT_FOUND, None))
                    } else {
                        eprintln!("Error: {msg}");
                        Ok((EXIT_ERROR, None))
                    }
                }
            }
        }
    }
}

/// Greedy token-budget selection: include nodes in PPR-rank order until the
/// next one would exceed the budget. Returns the count of nodes to take.
/// Token cost per node = (rendered length) / 4 — the standard cheap estimate.
fn token_budgeted_truncate(connected: &[nestweaver_engine::BrainNode], budget: usize) -> usize {
    let mut tokens = 0usize;
    let mut taken = 0usize;
    for n in connected {
        let cost = render_cost_tokens(n);
        if tokens + cost > budget {
            break;
        }
        tokens += cost;
        taken += 1;
    }
    taken
}

/// Apply age-decay score boost to non-Symbol nodes (CLI variant).
fn apply_recency_bias_cli(
    store: &nestweaver_store::GraphStore,
    nodes: &mut [nestweaver_engine::BrainNode],
    recency_weight: f64,
    recency_half_life_days: f64,
) {
    if recency_weight <= 0.0 {
        return;
    }
    let note_timestamps: std::collections::HashMap<String, f64> = store
        .list_notes(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| {
            n.modified_at
                .map(|t| (n.uid, nestweaver_engine::parse_iso8601_to_epoch(&t)))
        })
        .collect();
    let section_note_map: std::collections::HashMap<String, String> = store
        .list_all_sections()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.uid, s.note_uid))
        .collect();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    let ln2 = std::f64::consts::LN_2;
    let half_life_secs = recency_half_life_days * 86_400.0;

    for node in nodes.iter_mut() {
        if node.kind.to_lowercase().contains("symbol") {
            continue;
        }
        let modified_at_secs = if let Some(&ts) = note_timestamps.get(&node.uid) {
            ts
        } else if let Some(note_uid) = section_note_map.get(&node.uid) {
            note_timestamps.get(note_uid).copied().unwrap_or(0.0)
        } else {
            0.0
        };
        if modified_at_secs <= 0.0 {
            continue;
        }
        let age_secs = (now - modified_at_secs).max(0.0);
        let boost = 1.0 + recency_weight * (-(age_secs * ln2) / half_life_secs).exp();
        node.relevance *= boost;
    }

    nodes.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Rough token cost of rendering a single BrainNode line (chars / 4).
fn render_cost_tokens(n: &nestweaver_engine::BrainNode) -> usize {
    // Match the human-readable format below: "  0.1234  TITLE  [KIND]  LOCATION\n"
    let chars = 8 // "  0.XXXX  "
        + n.title.len()
        + 3 // "  ["
        + n.kind.len()
        + 1 // "]"
        + if n.location.is_empty() { 0 } else { 2 + n.location.len() } // "  LOCATION"
        + 1; // "\n"
    chars.div_ceil(4)
}

fn print_brain_context_text(result: &BrainContextResult, cut: usize, token_budget: Option<usize>) {
    println!("Seeds ({} resolved):", result.seeds.len());
    for n in &result.seeds {
        if n.location.is_empty() {
            println!("  {}  [{}]", n.title, n.kind);
        } else {
            println!("  {}  [{}]  {}", n.title, n.kind, n.location);
        }
    }

    if !result.unresolved_seeds.is_empty() {
        println!();
        println!("Unresolved seeds ({}):", result.unresolved_seeds.len());
        for s in &result.unresolved_seeds {
            println!("  {s}");
        }
    }

    if !result.connected.is_empty() {
        println!();
        let total = result.connected.len();
        let used_tokens: usize = result
            .connected
            .iter()
            .take(cut)
            .map(render_cost_tokens)
            .sum();
        match token_budget {
            Some(budget) => println!(
                "Connected ({} of {}, ~{}/{} tokens, ranked by relevance):",
                cut, total, used_tokens, budget
            ),
            None => println!("Connected ({} of {}, ranked by relevance):", cut, total),
        }
        for n in result.connected.iter().take(cut) {
            if n.location.is_empty() {
                println!("  {:.4}  {}  [{}]", n.relevance, n.title, n.kind);
            } else {
                println!(
                    "  {:.4}  {}  [{}]  {}",
                    n.relevance, n.title, n.kind, n.location
                );
            }
        }
    }
}

fn print_brain_context_json(result: &BrainContextResult, limit: usize) -> anyhow::Result<()> {
    let mut resp = serde_json::json!({
        "seeds_expanded": result.seeds.len(),
        "connected": result.connected.iter().take(limit).collect::<Vec<_>>(),
    });

    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = serde_json::json!(result.unresolved_seeds);
    }

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

/// Generate embeddings for all symbols that don't yet have one.
fn run_embed(
    db: Option<&Path>,
    endpoint: &str,
    model: &str,
    batch_size: usize,
) -> anyhow::Result<i32> {
    let store = open_store(db)?;

    let all_symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!(e))
        .context("list_all_symbols")?;

    let to_embed: Vec<_> = all_symbols
        .iter()
        .filter(|s| s.embedding.is_none())
        .collect();

    if to_embed.is_empty() {
        eprintln!("All symbols already have embeddings. Nothing to do.");
        return Ok(EXIT_SUCCESS);
    }

    eprintln!(
        "Generating embeddings for {} symbol(s) (skipping {} with existing embeddings) …",
        to_embed.len(),
        all_symbols.len() - to_embed.len()
    );

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut success_count = 0usize;
    let mut error_count = 0usize;

    for (batch_idx, batch) in to_embed.chunks(batch_size).enumerate() {
        let batch_start = batch_idx * batch_size + 1;
        let batch_end = (batch_start + batch.len() - 1).min(to_embed.len());
        eprintln!("  Batch {batch_start}–{batch_end} / {}", to_embed.len());

        for sym in batch {
            let text = if sym.signature.is_empty() {
                sym.name.clone()
            } else {
                sym.signature.clone()
            };

            match rt.block_on(generate_embedding(endpoint, model, &text)) {
                Ok(embedding) => {
                    if let Err(e) = store.update_symbol_embedding(&sym.uid, &embedding) {
                        eprintln!(
                            "    Warning: failed to store embedding for {}: {e}",
                            sym.uid
                        );
                        error_count += 1;
                    } else {
                        success_count += 1;
                    }
                }
                Err(e) => {
                    eprintln!("    Warning: embedding API error for {}: {e}", sym.uid);
                    error_count += 1;
                }
            }
        }
    }

    eprintln!("Done: {success_count} embedding(s) generated, {error_count} error(s).");

    if error_count > 0 {
        Ok(EXIT_ERROR)
    } else {
        Ok(EXIT_SUCCESS)
    }
}

fn run_instance(command: InstanceCommands) -> anyhow::Result<i32> {
    match command {
        InstanceCommands::Register { config_path } => {
            let config = nestweaver_engine::InstanceConfig::from_file(Path::new(&config_path))?;
            let registry_path = default_registry_path();
            let mut registry = nestweaver_engine::Registry::load_or_create(&registry_path)?;
            registry.register(&config.instance_id, &config_path)?;
            println!("Registered instance '{}'", config.instance_id);
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::List => {
            let registry = nestweaver_engine::Registry::load_or_create(&default_registry_path())?;
            if registry.list().is_empty() {
                println!("No instances registered.");
            } else {
                for entry in registry.list() {
                    println!("  {} -> {}", entry.id, entry.config_path);
                }
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Remove { id } => {
            let mut registry =
                nestweaver_engine::Registry::load_or_create(&default_registry_path())?;
            registry.remove(&id)?;
            println!("Removed instance '{id}'");
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Pull { id } => {
            let registry = nestweaver_engine::Registry::load_or_create(&default_registry_path())?;
            let entry = registry
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("instance '{}' not registered", id))?;
            let config =
                nestweaver_engine::InstanceConfig::from_file(Path::new(&entry.config_path))?;
            let backend = nestweaver_storage::create_backend(
                &config.snapshot_storage.backend,
                config.snapshot_storage.path.as_deref(),
            )?;
            let dest = dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("nestweaver")
                .join("snapshots")
                .join(&id);
            std::fs::create_dir_all(&dest)?;
            let meta = backend.pull_snapshot(&dest)?;
            println!("Pulled snapshot v{} for '{}'", meta.version, id);
            Ok(EXIT_SUCCESS)
        }
    }
}

fn run_snapshot(command: SnapshotCommands) -> anyhow::Result<i32> {
    match command {
        SnapshotCommands::Build { .. } => {
            eprintln!(
                "Use 'nestweaver index' to build a database, then package it with snapshot build"
            );
            eprintln!("Full instance-aware build not yet implemented.");
            process::exit(EXIT_ERROR);
        }
        SnapshotCommands::Verify { path } => {
            match nestweaver_engine::verify_snapshot(Path::new(&path)) {
                Ok(stamp) => {
                    println!("Snapshot verified OK");
                    println!("  Instance: {}", stamp.instance_id);
                    println!("  Engine: {}", stamp.engine_version);
                    println!("  Schema: {}", stamp.schema_hash_effective);
                    println!("  Embedding model: {}", stamp.embedding_model_id);
                    println!("  Built: {}", stamp.built_at);
                    println!("  Repos: {}", stamp.repos.len());
                    Ok(EXIT_SUCCESS)
                }
                Err(e) => {
                    eprintln!("Snapshot verification failed: {e}");
                    Ok(EXIT_ERROR)
                }
            }
        }
        SnapshotCommands::Push { .. } => {
            eprintln!("Not yet implemented");
            process::exit(EXIT_ERROR);
        }
    }
}

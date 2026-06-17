mod setup;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use miette::Diagnostic;
use nestweaver_engine::{
    BrainContextResult, BrainWatcher, CodeWatcher, ContextResult, DeadCodeConfidence,
    FeatureContextResult, HubNode, HybridSearchConfig, LookupResult, Summary, SummaryLevel,
    affected_tests, analyze_blast_radius, attach_cluster_ids, attach_communities,
    build_brain_context_hybrid_with_aliases, build_context_with_intent, build_feature_context,
    changed_files_from_git, compute_clusters, compute_cochanges, detect_implicit_projects,
    discover_cross_domain_links, embedding::generate_embedding, expand_query_with_aliases,
    export_cypher, export_graphml, export_in_memory_graph, export_mermaid, filter_by_target,
    find_bridge_nodes, find_hub_nodes, generate_agents_md_with_rules,
    generate_claude_md_with_rules, generate_cursor_rule_with_rules, generate_guide_with_rules,
    generate_repo_map, generate_summaries, get_last_indexed_at, incremental_index_with_name,
    index_directory_with_options, index_markdown_directory_since_with_ignore,
    index_markdown_directory_with_ignore, list_repos, list_services, load_alias_sidecar,
    load_clusters, load_extensions, load_manifest_cache, lookup_symbol, materialize_projects,
    record_last_indexed_at, render_text, save_clusters, save_cochange_sidecar, save_summaries,
    search_symbols, suggest_links, truncate_to_budget,
};
use nestweaver_schema::Symbol;
use nestweaver_store::{GraphStore, QueryIntent, TantivyIndex};

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

    /// Open the database directly for reads instead of routing through the daemon.
    /// Write operations always go through the daemon regardless of this flag.
    #[arg(long, global = true)]
    no_daemon: bool,
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

/// Format a Unix epoch timestamp (seconds since 1970-01-01) into
/// an ISO 8601 UTC string for human-readable display.
fn format_epoch_timestamp(epoch_secs: f64) -> String {
    let secs = epoch_secs as i64;
    // Days since Unix epoch using Howard Hinnant's civil-from-days algorithm.
    let mut days = secs.div_euclid(86400);
    let day_secs = secs.rem_euclid(86400);
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    days += 719_468;
    let era = if days >= 0 {
        days / 146_097
    } else {
        (days - 146_096) / 146_097
    };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Remove an indexed repository and all its data (symbols, files,
    /// services, contracts) from the graph.
    #[command(
        after_help = "Accepts a repo name, filesystem path, file:// URL, or UID.\n\nExamples:\n  nestweaver remove-repo acme-server\n  nestweaver remove-repo /home/user/dev/workspaces/acme/acme-server\n  nestweaver remove-repo repo:051a9ff9:abc123"
    )]
    RemoveRepo {
        /// Repo name, filesystem path, file:// URL, or UID
        target: String,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Remove a materialized project and its edges from the graph.
    #[command(
        after_help = "Accepts a project name or UID.\n\nExamples:\n  nestweaver remove-project acme\n  nestweaver remove-project proj:my-brain:abc123"
    )]
    RemoveProject {
        /// Project name or UID
        target: String,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Remove repos and vaults whose paths no longer exist on disk.
    #[command(
        after_help = "Scans all indexed repos and vaults, removing any whose source\ndirectory has been deleted or moved.\n\nExamples:\n  nestweaver prune-stale\n  nestweaver prune-stale --db ~/brain/.nestweaver/brain.lbug"
    )]
    PruneStale {
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
        #[arg(
            long,
            help = "Maximum number of results (default: 10, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
    /// Read a symbol's source span (start_line..end_line) — just the symbol,
    /// not the whole file. Optionally include adjacent symbols; token-budget aware.
    #[command(
        after_help = "Examples:\n  nestweaver read-symbols greet --neighbors 1\n  nestweaver read-symbols sym:... --token-budget 4000 --json"
    )]
    ReadSymbols {
        /// Symbol UIDs, names, or FQNs to read.
        #[arg(required = true)]
        targets: Vec<String>,
        #[arg(
            long,
            default_value = "0",
            help = "Include N adjacent symbols in the same file"
        )]
        neighbors: u8,
        #[arg(
            long = "token-budget",
            help = "Approximate token budget for the combined output"
        )]
        token_budget: Option<usize>,
        #[arg(
            long,
            help = "Repository root for resolving file paths (default: current dir)"
        )]
        root: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Regex search over indexed text (section bodies, note titles, symbol signatures)
    ///
    /// First-party replacement for shelling out to rg/grep. Uses a trigram
    /// pre-filter when built (`index --with-trigrams`), else falls back to
    /// scanning all candidate text — always correct, just slower.
    #[command(
        after_help = "Examples:\n  nestweaver regex-search 'fn\\s+authenticate'\n  nestweaver regex-search '(?i)todo' --path-prefix src/ --limit 20"
    )]
    RegexSearch {
        /// Rust regex pattern to search for
        pattern: String,
        #[arg(long = "path-prefix", help = "Restrict to file paths with this prefix")]
        path_prefix: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Restrict to node kinds (comma-separated): Section,Note,Symbol"
        )]
        kinds: Option<Vec<String>>,
        #[arg(long, help = "Maximum number of results")]
        limit: Option<usize>,
        #[arg(long = "max-millis", help = "Wall-clock time budget in milliseconds")]
        max_millis: Option<u64>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Count regex matches per pattern across indexed text (counts only)
    ///
    /// Companion to regex-search. Reports total matches, files matched, and the
    /// busiest files for each pattern. Reuses the same trigram pre-filter.
    #[command(
        after_help = "Examples:\n  nestweaver count-patterns 'TODO' 'FIXME'\n  nestweaver count-patterns '(?i)deprecated' --path-prefix src/"
    )]
    CountPatterns {
        /// One or more regex patterns to count
        #[arg(required = true)]
        patterns: Vec<String>,
        #[arg(long = "path-prefix", help = "Restrict to file paths with this prefix")]
        path_prefix: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Restrict to node kinds (comma-separated): Section,Note,Symbol"
        )]
        kinds: Option<Vec<String>>,
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
        #[arg(long, help = "Filter to symbols in this repo")]
        repo: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
        #[arg(long, help = "Filter to symbols in this repo")]
        repo: Option<String>,
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
        #[arg(
            long,
            help = "Display name override for the repo (avoids basename collisions when \
                    multiple repos share a generic name like 'client' or 'server')"
        )]
        name: Option<String>,
        #[arg(
            long = "with-trigrams",
            help = "Build a trigram posting table over indexed text to accelerate `regex-search` \
                    (opt-in storage cost)"
        )]
        with_trigrams: bool,
        #[arg(
            long = "with-git-activity",
            help = "Feature F12: mine git history and write a <db>.gitactivity.json recency \
                    sidecar so dormant code is demoted at rank-read time (opt-in)"
        )]
        with_git_activity: bool,
        #[arg(
            long,
            help = "Path to instance config (TOML). Honors per-repo [[repos]] use_git_activity \
                    opt-out for --with-git-activity"
        )]
        config: Option<PathBuf>,
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
        #[arg(long, help = "Maximum number of connected nodes to return")]
        limit: Option<usize>,
        #[arg(
            long,
            help = "Approximate token budget for output (takes precedence over --limit)"
        )]
        token_budget: Option<usize>,
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
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// List declared feature bundles from an instance config
    #[command(after_help = "Examples:\n  nestweaver list-features --config ./instance.toml")]
    ListFeatures {
        #[arg(long, help = "Path to instance config file")]
        config: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Analyze indexed repos and suggest cross-repo links and feature bundles
    #[command(
        after_help = "Examples:\n  nestweaver suggest-links --db ./all-repos.lbug\n  nestweaver suggest-links --config ./instance.toml"
    )]
    SuggestLinks {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Manage NestWeaver instances (register, list, remove, pull)
    Instance {
        #[command(subcommand)]
        command: InstanceCommands,
    },
    /// Brain: unified knowledge graph over markdown vaults
    ///
    /// Indexes a markdown vault into the graph alongside code repositories.
    /// Supports headings, sections, wikilinks, tags, PPR-based context
    /// retrieval, topic clustering, memory tiers, and live file watching.
    Brain {
        #[command(subcommand)]
        command: Box<BrainCommands>,
    },
    /// F11 memory-bank operations over the vault: typed-relationship health
    /// (`lint`), tier-promotion proposals (`consolidate`), and typed-edge
    /// traversal (`related`).
    Memory {
        #[command(subcommand)]
        command: Box<MemoryCommands>,
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
        #[arg(
            long,
            value_delimiter = ',',
            help = "Comma-separated list of tool names to expose (default: all)"
        )]
        tools: Option<Vec<String>>,
        #[arg(
            long,
            help = "Record interaction telemetry to a sidecar file for usage-based ranking"
        )]
        track_interactions: bool,
        /// Path to instance config (TOML) for [limits], [response], [ranking] settings.
        /// In daemon mode, the daemon's own --config takes precedence.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Open the database directly for reads instead of routing through the daemon.
        /// Write operations always go through the daemon regardless of this flag.
        #[arg(long)]
        no_daemon: bool,
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

        /// Enable live re-indexing watchers (file system watching)
        #[arg(long)]
        watch: bool,
    },
    /// Manage interaction memory
    Interactions {
        #[command(subcommand)]
        command: InteractionCommands,
    },
    /// Feature F17 — lightweight result reranker (off-by-default heuristic).
    ///
    /// The reranker reorders the top-N of an already-retrieved set; it does NOT
    /// change recall. The default scorer is a transparent MONOTONIC heuristic,
    /// not a validated nDCG win. A learned model is only trustworthy after the
    /// eval harness + accumulated labels gate it at >= 5% nDCG@10. This command
    /// group exposes the offline-training-export scaffold.
    Rerank {
        #[command(subcommand)]
        command: RerankCommands,
    },
    /// Inspect the API contract graph (F2-core).
    ///
    /// Contracts are HTTP routes / gRPC methods / GraphQL operations derived
    /// from spec files and framework handlers. Links are HYPOTHESES, not
    /// ground truth — see the confidence scores.
    Contracts {
        #[command(subcommand)]
        command: ContractCommands,
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
            help = "Output format: markdown (default), skill, cursor-rule, agents-md, claude-md"
        )]
        format: String,
        #[arg(
            long,
            value_name = "FILE",
            help = "Override the built-in hard rules from a TOML ([[rules]]) or markdown file"
        )]
        rules_from: Option<PathBuf>,
    },
    /// Admin: subagent guidance instruction store and runtime hook installation.
    ///
    /// Injected guidance HELPS but is NOT enforcement — instruction-following
    /// by an LLM is probabilistic (Geng et al. 2025, "Control Illusion"). Hook
    /// JSON schemas are Claude-Code-specific.
    Admin {
        #[command(subcommand)]
        command: AdminCommands,
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
            help = "Resolution parameter (higher = smaller clusters) [default: 0.5, or 0.3 for large graphs >10K symbols]"
        )]
        resolution: Option<f64>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
        after_help = "Examples:\n  nestweaver list-projects\n  nestweaver list-projects --config ./instance.toml --json"
    )]
    ListProjects {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Materialize declared projects, wiki sources, and cross-repo links
    MaterializeProjects {
        #[arg(long, help = "Path to instance config (TOML)")]
        config: PathBuf,
        #[arg(long, help = "Path to the database file")]
        db: Option<PathBuf>,
    },

    /// Detect implicit projects from vault structure and code patterns
    DetectImplicitProjects {
        #[arg(long, help = "Path to vault directory")]
        vault: PathBuf,
        #[arg(long, help = "Path to the database file")]
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
    /// F10: orient on a topic in one call — architectural map + bundle_id
    ///
    /// Runs hybrid retrieval (PPR + BM25 + PRF) for the query, groups results into
    /// architectural domains, inlines a few high-confidence bodies, and persists a
    /// bundle (24h TTL). Drill in afterwards with `investigate-expand` /
    /// `investigate-hydrate`.
    #[command(
        after_help = "Examples:\n  nestweaver investigate \"device pairing\"\n  nestweaver investigate \"how indexing works\" --scope repo:nestweaver --token-budget 8000 --json"
    )]
    Investigate {
        /// Topic / feature / subsystem to orient on
        query: String,
        #[arg(
            long,
            help = "Scope: project:<slug>, repo:<name>, or vault/all (default = no restriction)"
        )]
        scope: Option<String>,
        #[arg(
            long,
            default_value = "4000",
            help = "Approximate token budget (chars/4)"
        )]
        token_budget: usize,
        #[arg(long, help = "Filesystem root for inline bodies (default: repo root)")]
        root: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// F10: drill into investigate bundle entries (full body + neighbors)
    #[command(
        after_help = "Examples:\n  nestweaver investigate-expand bndl_abc --targets a123,sym:foo"
    )]
    InvestigateExpand {
        /// Bundle id returned by `investigate`
        bundle_id: String,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Comma-separated asset_ids (from the map) or node uids to expand"
        )]
        targets: Vec<String>,
        #[arg(long, help = "Filesystem root for source bodies (default: repo root)")]
        root: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },

    /// F10: fill in bodies/summaries for all un-hydrated bundle entries
    #[command(after_help = "Examples:\n  nestweaver investigate-hydrate bndl_abc")]
    InvestigateHydrate {
        /// Bundle id returned by `investigate`
        bundle_id: String,
        #[arg(
            long,
            default_value = "4000",
            help = "Approximate token budget (chars/4)"
        )]
        token_budget: usize,
        #[arg(long, help = "Filesystem root for source bodies (default: repo root)")]
        root: Option<PathBuf>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
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
        /// Deprecated: daemon mode always allows writes. Kept for backward compatibility.
        #[arg(long, hide = true)]
        allow_writes: bool,
        /// Overwrite existing skill/guide files even if customized
        #[arg(long)]
        force: bool,
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
        after_help = "Examples:\n  nestweaver export --format cypher\n  nestweaver export --format graphml --output graph.xml\n  nestweaver export --format mermaid --top 30\n  nestweaver export --format msgpack --output graph.msgpack"
    )]
    Export {
        #[arg(
            long,
            default_value = "cypher",
            help = "Output format: cypher, graphml, mermaid, msgpack"
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

    /// Select the test files an MR should run for a set of code changes (F13).
    ///
    /// Static, call-graph-based regression test selection: maps changed files
    /// to their symbols, reverse-traverses CALLS/IMPORTS to depth 3, and buckets
    /// dependent test files into priority tiers. This is a prioritized signal,
    /// NOT a provably-safe subset — it misses reflection, DI, codegen, and
    /// data-driven/integration tests. "No tests found" is NOT safe-to-skip.
    #[command(
        name = "affected-tests",
        after_help = "Examples:\n  nestweaver affected-tests --files src/auth.rs,src/db.rs\n  nestweaver affected-tests --base-ref main\n  nestweaver affected-tests --base-ref main --json"
    )]
    AffectedTests {
        #[arg(
            long,
            help = "Comma-separated changed file paths (repo-relative)",
            conflicts_with = "base_ref"
        )]
        files: Option<String>,
        #[arg(
            long = "base-ref",
            help = "Git ref to diff against (e.g. main); uses git diff --name-only base...HEAD"
        )]
        base_ref: Option<String>,
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
        /// Path to the repository to watch (auto-detects if omitted)
        #[arg(help = "Repository path to watch")]
        repo: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Instance ID (for multi-instance setups)")]
        instance: Option<String>,
        #[arg(long, help = "Re-fetch wiki sources every N hours (requires --config)")]
        refresh_wiki_hours: Option<u64>,
        #[arg(
            long,
            help = "Path to instance config (TOML) — required for --refresh-wiki-hours"
        )]
        config: Option<PathBuf>,
    },
    /// Inspect per-path ranking priors (Feature F6).
    Ranking {
        #[command(subcommand)]
        command: RankingCommands,
    },
    /// P0.3 — offline retrieval-quality evaluation harness.
    ///
    /// Scores retrieval (nDCG@10 / MRR / precision@5) over a JUDGED query set
    /// so the off-by-default quality features (F6/F7/F1/F12/F17) can be
    /// MEASURED before being trusted.
    ///
    /// HONEST FRAMING: meaningful evaluation needs REAL human relevance labels
    /// over the actual corpus you index. The bundled sample file is a FORMAT
    /// TEMPLATE, not a benchmark; metrics on a tiny/synthetic set are not
    /// authoritative. Look at per-query win/loss + confidence and use
    /// time/query-based splits — not just the mean — before trusting a small
    /// delta.
    #[command(
        after_help = "Examples:\n  nestweaver eval run --queries ./eval-queries.jsonl\n  nestweaver eval run --queries ./eval-queries.jsonl --json --prf\n  nestweaver eval compare --queries ./eval-queries.jsonl --prf\n\nFormat template + guide: examples/eval/ (eval-queries.example.jsonl, README.md)"
    )]
    Eval {
        #[command(subcommand)]
        command: EvalCommands,
    },
    /// Manage the NestWeaver daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
        /// Path to the database file
        #[arg(
            long,
            global = true,
            help = "Path to the database file [env: NESTWEAVER_DB]"
        )]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (usually auto-started on first use)
    Start {
        /// Idle timeout in seconds
        #[arg(long, default_value = "3600")]
        idle_timeout: u64,
        /// Optional path to `nestweaver-instance.toml`. When supplied, the
        /// daemon loads `[ranking]`, `[response]`, and other instance settings
        /// once at startup so RPCs (e.g. `brain_search`) apply them with
        /// parity to the direct-disk CLI path.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Hidden: redirect users who pass this flag here to the correct command.
        #[arg(long, hide = true)]
        track_interactions: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Show daemon status
    Status,
    /// Stop and restart the daemon
    Restart {
        /// Idle timeout in seconds
        #[arg(long, default_value = "3600")]
        idle_timeout: u64,
        /// Optional path to `nestweaver-instance.toml`, forwarded to the
        /// `daemon start` invocation so restarts preserve F6 `[ranking]`
        /// priors and other instance settings.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Subcommands under `nestweaver eval`.
#[derive(Subcommand)]
enum EvalCommands {
    /// Run the harness once over a judged query set and print metrics.
    ///
    /// Prints the aggregate (mean nDCG@10 / MRR / precision@5) plus a per-query
    /// table. With `--json`, prints the full `EvalReport` instead.
    Run {
        /// Path to the judged-query file (JSON array or JSONL of
        /// {query, intent?, relevance} objects). REQUIRED.
        #[arg(long, help = "Judged-query file (JSON array or JSONL)")]
        queries: PathBuf,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Output the EvalReport as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Enable Feature F7 pseudo-relevance feedback on the BM25 leg (off by default)"
        )]
        prf: bool,
        #[arg(
            long,
            help = "Enable Feature F17 reranking of the top-N before scoring (off by default)"
        )]
        rerank: bool,
    },
    /// Run the SAME judged set twice — baseline vs a toggled feature — and print
    /// the mean nDCG@10 delta plus per-query win/loss counts.
    ///
    /// Pass `--prf` and/or `--rerank` to choose which feature to toggle ON in
    /// the treatment run (baseline always has it OFF). Judge the result against
    /// the >= 5% nDCG@10 gate — but remember a small mean delta on a small set
    /// is NOT authoritative; inspect the win/loss counts too.
    Compare {
        /// Path to the judged-query file (JSON array or JSONL). REQUIRED.
        #[arg(long, help = "Judged-query file (JSON array or JSONL)")]
        queries: PathBuf,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Output the EvalComparison as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Toggle: compare PRF-on (treatment) vs PRF-off (baseline)"
        )]
        prf: bool,
        #[arg(
            long,
            help = "Toggle: compare rerank-on (treatment) vs rerank-off (baseline)"
        )]
        rerank: bool,
    },
}

/// Subcommands under `nestweaver ranking`.
#[derive(Subcommand)]
enum RankingCommands {
    /// Dry-run: show how the configured `[ranking]` priors would rescale a
    /// single node's relevance. Looks up the node's file-path location, finds
    /// the last matching glob rule (last-match-wins), and prints the math.
    Explain {
        /// UID of the node to explain (e.g. `sym:...`, `note:...`).
        uid: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML) carrying [ranking]")]
        config: Option<PathBuf>,
        #[arg(
            long,
            default_value = "1.0",
            help = "Base relevance to apply the prior against"
        )]
        base_relevance: f64,
    },
    /// Feature F12: explain how git-activity recency dampens a symbol's
    /// CodeRank. Prints `base_pagerank`, `git_activity_score`, and `final_rank`
    /// (= base × clamped recency multiplier). Neutral (multiplier 1.0) when no
    /// `<db>.gitactivity.json` sidecar is loaded for the symbol's file.
    Rank {
        /// UID or name of the symbol to explain.
        uid: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to instance config (TOML) carrying [ranking] git_activity_weight"
        )]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum BrainCommands {
    /// Index a markdown vault into the brain. Auto-detects Obsidian vault
    /// (.obsidian/ present) vs plain markdown folder.
    Add {
        /// Path to the vault directory.
        path: PathBuf,
        #[arg(long, help = "Friendly name for the vault (default: directory name)")]
        name: Option<String>,
        #[arg(long, help = "Instance ID (overrides --config)")]
        instance: Option<String>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to instance config (TOML) — uses its instance_id and db_path"
        )]
        config: Option<PathBuf>,
        #[arg(long, help = "Additional glob patterns to ignore (comma-separated)")]
        ignore: Option<String>,
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Check if the indexed graph is stale by comparing each repo's
    /// indexed SHA against git HEAD.
    StaleCheck {
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
        #[arg(long, help = "Additional glob patterns to ignore (comma-separated)")]
        ignore: Option<String>,
        #[arg(long, help = "Re-fetch wiki sources every N hours (requires --config)")]
        refresh_wiki_hours: Option<u64>,
        #[arg(
            long,
            help = "Path to instance config (TOML) — required for --refresh-wiki-hours"
        )]
        config: Option<PathBuf>,
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
        #[arg(long, help = "Additional glob patterns to ignore (comma-separated)")]
        ignore: Option<String>,
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
        #[arg(
            long,
            help = "Maximum results (default: 20, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
        /// Feature F7: pseudo-relevance-feedback query expansion. Mines
        /// high-IDF terms from the top hits and re-runs BM25 with them
        /// down-weighted to improve recall. Off by default.
        #[arg(
            long = "prf",
            help = "Enable pseudo-relevance-feedback query expansion (Feature F7)"
        )]
        prf: bool,
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
        #[arg(
            long,
            help = "Maximum connected results (default: 30, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
        /// Filter results to nodes whose kind starts with one of these values
        /// (e.g. Symbol, Note, Section, Tag, Heading). Accepts comma-separated
        /// values or repeated `--kinds` flags.
        #[arg(
            long = "kinds",
            value_delimiter = ',',
            help = "Keep only nodes with these kind prefixes (e.g. Symbol,Note)"
        )]
        kinds: Vec<String>,
        /// Filter results to nodes associated with these repo UIDs or names.
        /// Accepts comma-separated values or repeated `--repos` flags.
        #[arg(
            long = "repos",
            value_delimiter = ',',
            help = "Keep only nodes from these repo UIDs or names"
        )]
        repos: Vec<String>,
        /// Filter results to nodes associated with these vault UIDs or names.
        /// Accepts comma-separated values or repeated `--vaults` flags.
        #[arg(
            long = "vaults",
            value_delimiter = ',',
            help = "Keep only nodes from these vault UIDs or names"
        )]
        vaults: Vec<String>,
        /// Keep only nodes whose location (file path) starts with this prefix.
        #[arg(
            long = "path-prefix",
            help = "Keep only nodes whose location starts with this prefix"
        )]
        path_prefix: Option<String>,
        /// Include only nodes tagged with any of these tags (note/section nodes
        /// only). Accepts comma-separated values or repeated `--tags` flags.
        #[arg(
            long = "tags",
            value_delimiter = ',',
            help = "Keep only note/section nodes tagged with any of these tags"
        )]
        tags: Vec<String>,
        /// Exclude nodes tagged with any of these tags (note/section nodes
        /// only). Accepts comma-separated values or repeated `--exclude-tags`
        /// flags.
        #[arg(
            long = "exclude-tags",
            value_delimiter = ',',
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
        /// Feature F8: embed each high-relevance result's source body inline so
        /// the agent can skip a follow-up read. Off by default.
        #[arg(
            long = "inline-bodies",
            help = "Embed high-relevance result bodies inline (Feature F8)"
        )]
        inline_bodies: bool,
        /// Repo root for resolving Symbol bodies when --inline-bodies is set
        /// (default: current dir). Note/Section bodies come from the store and
        /// ignore this.
        #[arg(long, help = "Repo root for inline Symbol bodies (default: cwd)")]
        root: Option<PathBuf>,
        /// Feature F7: pseudo-relevance-feedback query expansion on the BM25
        /// leg. Mines high-IDF terms from the top hits and re-runs BM25 with
        /// them down-weighted to improve recall. Off by default.
        #[arg(
            long = "prf",
            help = "Enable pseudo-relevance-feedback query expansion (Feature F7)"
        )]
        prf: bool,
        /// Feature F17: rerank the top-N retrieved candidates before
        /// truncation. OFF by default; behavior is byte-identical when off.
        /// Uses a hand-tuned MONOTONIC heuristic scorer (an unvalidated
        /// reordering, NOT a proven nDCG win) unless an optional learned-weights
        /// file `<db>.rerank.json` is present and version-matched. Reranking
        /// only reorders an already-retrieved set; recall is unchanged.
        #[arg(
            long = "rerank",
            help = "Rerank the top-N retrieved candidates (Feature F17, heuristic, off by default)"
        )]
        rerank: bool,
        /// Query intent override that tunes PPR's damping factor and edge
        /// weights. Same accepted values as `nestweaver context --intent`.
        /// When omitted, the engine auto-detects an intent from the seed
        /// kinds. Mirrors the lower-level `context --intent` flag so callers
        /// can force a specific traversal profile against the unified brain.
        #[arg(
            long = "intent",
            help = "Query intent override: find-definition, understand-architecture, analyze-impact, blast-radius, general-context"
        )]
        intent: Option<String>,
        /// Hard-filter test-path nodes out of both seeds and connected
        /// results. Distinct from the existing soft test-path deboost
        /// multiplier — this drops the rows entirely. Useful when callers
        /// want production-only context (e.g. for diffs that should not
        /// surface fixtures or playwright specs).
        #[arg(
            long = "no-tests",
            help = "Hard-filter test-path nodes out of seeds + connected results (in addition to the soft deboost)"
        )]
        no_tests: bool,
        /// Prefer a specific instance_id when ranking. When the database
        /// contains rows from multiple instance_ids (e.g. mid-merge), this
        /// scopes results to nodes registered under the given instance.
        /// Pass `--prefer-instance <id>` to scope to that instance only.
        #[arg(
            long = "prefer-instance",
            help = "Scope ranking to nodes registered under this instance_id"
        )]
        prefer_instance: Option<String>,
    },
    /// List wikilinks whose target is ambiguous or low-confidence
    /// (confidence < 1.0), with suggested target notes for each.
    BrokenLinks {
        #[arg(long, default_value = "5", help = "Max suggested targets per link")]
        max_suggestions: usize,
        #[arg(
            long,
            help = "Max broken links to return (default: 50, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// List notes with zero inbound and zero outbound wikilinks. Index/MOC
    /// notes are excluded via a default allowlist (override with --allow).
    Orphans {
        #[arg(long, help = "Restrict to this vault UID")]
        vault: Option<String>,
        #[arg(
            long = "path-prefix",
            help = "Restrict to notes under this path prefix"
        )]
        path_prefix: Option<String>,
        #[arg(
            long = "allow",
            help = "Note path/title to exclude (repeatable; overrides the default allowlist)"
        )]
        allow: Vec<String>,
        #[arg(
            long,
            help = "Max orphan documents to return (default: 50, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Detect topic clusters by running Leiden community detection over the
    /// note-to-note wikilink graph. Each cluster is labelled by its most
    /// central member.
    TopicClusters {
        #[arg(long, default_value = "0.5", help = "Leiden resolution")]
        resolution: f64,
        #[arg(
            long,
            help = "Max clusters to return (default: 50, or [limits].default_result_limit from config)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Show a tag's note count and the tags that co-occur with it.
    /// Omit the tag to dump the whole tag co-occurrence graph (all tags).
    TagGraph {
        /// Optional focus tag (with or without leading #). When omitted,
        /// prints the full tag co-occurrence graph for every tag.
        tag: Option<String>,
        #[arg(
            long,
            help = "Max tags to return (default: 50, or [limits].default_result_limit from config). Ignored when a specific tag is queried."
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// One-shot health summary of the vault's document graph: note/wikilink
    /// counts, broken links, orphans, average out-degree, top tags, and notes
    /// by year.
    DocStats {
        #[arg(long, default_value = "10", help = "Max entries in top_tags")]
        top_tags_limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum MemoryCommands {
    /// Run the seven F11 memory-bank health checks over the vault: stale
    /// notes, Supersedes contradictions, orphans, broken wikilinks,
    /// supersession chains, schema drift, and dangling relationships.
    Lint {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Propose tier promotions (daily logs → ideas → project files).
    /// DRY-RUN by default; set `--apply` to move files to their promoted destinations.
    Consolidate {
        #[arg(
            long,
            help = "Move files to their promoted destinations (default is dry-run)"
        )]
        apply: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
    },
    /// Walk the typed-relationship graph (Supersedes/DependsOn/CausedBy/
    /// RelatesTo) from a note, excluding generic wikilinks.
    Related {
        /// Seed note UID to traverse from.
        uid: String,
        #[arg(
            long = "edge-type",
            help = "Edge type to follow (repeatable; default: all four)"
        )]
        edge_types: Vec<String>,
        #[arg(long, default_value = "2", help = "Max BFS depth")]
        depth: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
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
    /// Remove a registered instance. With `--purge-graph`, also
    /// cascade-delete every Repo/File/Symbol/Vault/Note/Project owned by
    /// the instance from the graph database. Useful for cleaning up a
    /// ghost instance left behind by a misconfigured `instance merge`.
    Remove {
        /// Instance ID to remove
        id: String,
        /// Also cascade-delete the instance's data from the graph
        /// database (Repos and their children, Vaults and their notes,
        /// Projects). When set, missing registry entries are tolerated
        /// so ghost instances can be cleaned up.
        #[arg(long)]
        purge_graph: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Pull the latest snapshot for an instance
    Pull {
        /// Instance ID to pull
        id: String,
    },
    /// Merge one instance into another by rewriting vault, project, and
    /// repo rows. Use this to recover from misconfigured deployments
    /// where brain add was run with the wrong --instance.
    Merge {
        /// Source instance ID to merge from
        #[arg(long)]
        from: String,
        /// Target instance ID to merge into
        #[arg(long)]
        to: String,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AdminCommands {
    /// Print or manage the agent instruction store.
    ///
    /// With no flag, prints the main instructions. The stores live at
    /// `~/.nestweaver/instructions.md` and `~/.nestweaver/instructions.subagent.md`.
    ///
    /// NOTE: injected guidance helps but is NOT enforcement — an LLM follows
    /// instructions probabilistically (Geng et al. 2025).
    Instructions {
        #[arg(
            long,
            help = "Print the subagent guidance to stdout (single clean output, hook-friendly)"
        )]
        for_subagent: bool,
        #[arg(
            long,
            value_name = "FILE",
            help = "Install FILE as the instruction store (subagent store when --for-subagent is set)"
        )]
        set: Option<PathBuf>,
        #[arg(long, help = "Reset both stores to the bundled defaults")]
        reset: bool,
    },
    /// Install a runtime hook that injects subagent guidance.
    ///
    /// For the `claude` runtime, adds a PreToolUse hook on the `Task` matcher
    /// that runs `nestweaver admin instructions --for-subagent`. Hook JSON
    /// schemas are Claude-Code-specific.
    InstallHook {
        #[arg(
            long,
            default_value = "claude",
            help = "Target runtime (only 'claude' is supported in this version)"
        )]
        runtime: String,
        #[arg(
            long,
            help = "Print the JSON patch that would be applied, without writing"
        )]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SnapshotCommands {
    /// Build a snapshot from the current graph
    Build {
        #[arg(long, help = "Instance ID to build snapshot for")]
        instance: Option<String>,
        /// Path to the database file
        #[arg(long)]
        db: Option<PathBuf>,
        /// Path to the instance config (instance.toml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Output directory for the snapshot [default: next to the database]
        #[arg(long)]
        output: Option<PathBuf>,
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
        #[arg(long, help = "Path to instance config (TOML)")]
        config: Option<PathBuf>,
        #[arg(long, help = "Path to the snapshot directory to push")]
        snapshot_dir: Option<PathBuf>,
        #[arg(long, help = "Storage backend name (local, s3, gitlab)")]
        backend: Option<String>,
        #[arg(long, help = "Storage backend path")]
        backend_path: Option<String>,
    },
}

#[derive(Subcommand)]
enum ContractCommands {
    /// List API contracts derived from spec files and framework handlers.
    List {
        #[arg(long, help = "Filter to a single repo by name or UID")]
        repo: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show contract drift: routes declared in a spec but not implemented,
    /// and routes implemented by a handler but declared in no spec.
    Drift {
        #[arg(long, help = "Filter to a single repo by name or UID")]
        repo: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RerankCommands {
    /// SCAFFOLD: export per-candidate feature+label rows (JSONL) derived from
    /// F1 interaction success signals (TerminalSuccess/FollowUp → positive) for
    /// OFFLINE training elsewhere. This does NOT train a model — there is no
    /// labelled data of meaningful size yet and no eval harness to gate one. It
    /// exports whatever interaction data exists (possibly empty). A future
    /// external trainer would consume this JSONL and emit a `<db>.rerank.json`
    /// weights file, which must beat the monotonic baseline by >= 5% nDCG@10 on
    /// the (not-yet-built) eval harness before being trusted.
    ExportTraining {
        /// Output JSONL path (default: `<db>.rerank-training.jsonl`).
        #[arg(long, help = "Output JSONL path")]
        out: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum InteractionCommands {
    /// Show interaction memory statistics
    Status {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Clear all interaction memory
    Clear {
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
    },
    /// Show recorded interaction events / decayed score for a UID, or the
    /// top UIDs by a given event kind.
    Show {
        /// UID to inspect.
        #[arg(long)]
        uid: Option<String>,
        /// List the top N UIDs (by `--kind`) instead of a single UID.
        #[arg(long)]
        top: Option<usize>,
        /// Event kind to rank by with `--top`: access, query, follow_up,
        /// impact, terminal_success, or score (default).
        #[arg(long, default_value = "score")]
        kind: String,
        #[arg(
            long,
            help = "Path to the database file [env: NESTWEAVER_DB] [default: ./nestweaver.lbug]"
        )]
        db: Option<PathBuf>,
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

/// Resolve the DB path for a read command, honoring `--config`.
///
/// Precedence: an explicit `--db` always wins; otherwise the instance
/// config's `db` field (when `--config` is given and declares one); otherwise
/// `NESTWEAVER_DB` / the default. This is what makes `--config` actually
/// select a DB instead of being silently ignored (Bug #19).
fn resolve_db_with_config(db: Option<PathBuf>, config: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(db) = db {
        return Ok(db);
    }
    if let Some(cfg_path) = config {
        let cfg = nestweaver_engine::InstanceConfig::from_file(cfg_path)
            .with_context(|| format!("loading --config {}", cfg_path.display()))?;
        if let Some(db) = cfg.db_path() {
            return Ok(db);
        }
    }
    Ok(default_db_path())
}

/// Load an optional instance config for `[ranking]`/`[response]` settings.
/// Returns `None` when no path is given; when a path IS given but fails to
/// parse, warns and returns `None` — so a typo'd `--config` doesn't silently
/// disable ranking priors / inline-body tuning.
fn load_instance_config_opt(path: Option<&Path>) -> Option<nestweaver_engine::InstanceConfig> {
    let p = path?;
    match nestweaver_engine::InstanceConfig::from_file(p) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!(
                "warning: --config {} failed to load ({e}); ranking/response settings ignored",
                p.display()
            );
            None
        }
    }
}

/// Resolve a CLI `--limit` value: explicit flag > instance config > built-in default.
fn resolve_limit(
    explicit: Option<usize>,
    config: Option<&nestweaver_engine::InstanceConfig>,
    builtin_default: usize,
) -> usize {
    explicit
        .or_else(|| config.map(|c| c.limits.default_result_limit))
        .unwrap_or(builtin_default)
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
    let store = GraphStore::open_read_only(path).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("Corrupted wal") || msg.contains("Could not set lock") {
            anyhow::anyhow!(
                "The NestWeaver daemon already has the database open at {}.\n\
                 This command should route through the daemon automatically. \
                 If you see this error, please report it as a bug.\n\
                 Workaround: pass --no-daemon to open the database directly \
                 (only safe when the daemon is stopped).",
                path.display()
            )
        } else {
            anyhow::anyhow!("failed to open database at {}: {e}", path.display())
        }
    })?;

    let pr_path = path.with_extension("pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);

    // Load interaction memory scores so PPR can apply a small bias toward
    // frequently-accessed nodes.
    if let Some(scores) = nestweaver_engine::load_interaction_scores(path) {
        store.load_interaction_cache(scores);
    }

    // Feature F12: load the git-activity recency sidecar (if present) so
    // ranking/hubs demote dormant code at read time. Absent → neutral.
    let ga_path = nestweaver_engine::sidecar_path(path, ".gitactivity.json");
    let _ = store.load_git_activity_sidecar(&ga_path);

    Ok(store)
}

/// Stop the daemon to acquire the write lock for direct indexing.
///
/// Only used in the `!use_daemon` fallback (test/CI via `NESTWEAVER_NO_DAEMON=1`).
/// In production, all writes route through daemon RPCs and this function is never called.
fn stop_daemon_if_running(db_path: &Path) -> bool {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
    let was_running = pidfile.exists()
        && nestweaver_client::autostart::read_pid(&pidfile)
            .is_some_and(nestweaver_client::autostart::is_process_alive);
    if was_running {
        eprintln!("Stopping daemon to acquire write lock (will restart after)...");
        if let Some(pid) = nestweaver_client::autostart::read_pid(&pidfile) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if !nestweaver_client::autostart::is_process_alive(pid) {
                    break;
                }
            }
        }
    }
    was_running
}

/// Restarts the daemon after a direct-write operation.
/// Only used in the `!use_daemon` fallback (test/CI).
fn restart_daemon(db_path: &Path, config: Option<&Path>) {
    eprintln!("Restarting daemon...");
    if let Err(e) = nestweaver_client::autostart::ensure_daemon(db_path, config) {
        eprintln!("Warning: failed to restart daemon: {e}");
    }
}

fn tantivy_sidecar_path_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".tantivy");
    PathBuf::from(s)
}

/// Parse the `--ignore` CLI flag (comma-separated glob patterns) into a
/// `Vec<String>`. Returns an empty vec when the flag is `None`.
fn parse_ignore_flag(flag: &Option<String>) -> Vec<String> {
    match flag {
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        None => Vec::new(),
    }
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
    let use_daemon = !cli.no_daemon && std::env::var("NESTWEAVER_NO_DAEMON").is_err();
    match cli.command {
        Commands::ListRepos {
            instance,
            json,
            db,
            config: _,
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({});
                if let Some(ref inst) = instance {
                    args["instance"] = serde_json::json!(inst);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "list_repos", args) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        let repos: Vec<nestweaver_schema::Repo> =
                            serde_json::from_value(value).unwrap_or_default();
                        if repos.is_empty() {
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
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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

        Commands::RemoveRepo { target, db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            let store = GraphStore::open_read_only(&db_path)
                .with_context(|| format!("failed to open database at {}", db_path.display()))?;

            let repos = store.list_repos(None).context("failed to list repos")?;

            // Resolve target → repo UID.  Accept: UID, name, path, or URL.
            let canonical_target = std::fs::canonicalize(&target)
                .map(|p| format!("file://{}", p.display()))
                .unwrap_or_default();

            let url_target = if target.starts_with("file://") {
                target.clone()
            } else if std::path::Path::new(&target).is_absolute() {
                format!("file://{target}")
            } else {
                String::new()
            };

            let matched: Vec<&nestweaver_schema::Repo> = repos
                .iter()
                .filter(|r| {
                    r.uid == target
                        || r.name.as_deref() == Some(&target)
                        || r.url == url_target
                        || r.url == canonical_target
                        || r.url.ends_with(&format!("/{target}"))
                })
                .collect();

            if matched.is_empty() {
                eprintln!(
                    "Error: no repo matching '{target}' found.\n  \
                     Run `nestweaver list-repos` to see indexed repos."
                );
                return Ok((EXIT_NOT_FOUND, None));
            }
            if matched.len() > 1 {
                eprintln!("Error: '{target}' matches multiple repos:");
                for r in &matched {
                    eprintln!("  {} ({})", r.uid, r.name.as_deref().unwrap_or(&r.url));
                }
                eprintln!("Re-run with the full UID to disambiguate.");
                return Ok((EXIT_ERROR, None));
            }

            let repo = matched[0];
            let display_name = repo.name.as_deref().unwrap_or(&repo.url);

            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            match rt.block_on(client.remove_repo(&repo.uid)) {
                Ok(resp) => {
                    println!(
                        "Removed repo '{}' ({} file(s), {} symbol(s) deleted).",
                        display_name, resp.files_deleted, resp.symbols_deleted
                    );
                }
                Err(e) => {
                    eprintln!("Error: failed to remove repo '{}': {e}.", repo.uid);
                    return Ok((EXIT_ERROR, None));
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::RemoveProject { target, db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            let store = GraphStore::open_read_only(&db_path)
                .with_context(|| format!("failed to open database at {}", db_path.display()))?;

            let projects = store.list_projects().context("failed to list projects")?;

            let matched: Vec<&nestweaver_schema::Project> = projects
                .iter()
                .filter(|r| r.uid == target || r.name.eq_ignore_ascii_case(&target))
                .collect();

            if matched.is_empty() {
                eprintln!(
                    "Error: no project matching '{target}' found.\n  \
                     Run `nestweaver list-projects` to see materialized projects."
                );
                return Ok((EXIT_NOT_FOUND, None));
            }
            if matched.len() > 1 {
                eprintln!("Error: '{target}' matches multiple projects:");
                for p in &matched {
                    eprintln!("  {} ({})", p.uid, p.name);
                }
                eprintln!("Re-run with the full UID to disambiguate.");
                return Ok((EXIT_ERROR, None));
            }

            let project = matched[0];
            let display_name = &project.name;

            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            match rt.block_on(client.remove_project(&project.uid)) {
                Ok(_resp) => {
                    println!("Removed project '{display_name}'.");
                }
                Err(e) => {
                    eprintln!("Error: failed to remove project '{}': {e}.", project.uid);
                    return Ok((EXIT_ERROR, None));
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::PruneStale { db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            match rt.block_on(client.prune_stale()) {
                Ok(resp) => {
                    let total = resp.removed_repos.len() + resp.removed_vaults.len();
                    if total == 0 {
                        println!("No stale sources found.");
                    } else {
                        for name in &resp.removed_repos {
                            println!("  Removed repo: {name}");
                        }
                        for name in &resp.removed_vaults {
                            println!("  Removed vault: {name}");
                        }
                        println!("Pruned {total} stale source(s).");
                    }
                }
                Err(e) => {
                    eprintln!("Error: prune failed: {e}");
                    return Ok((EXIT_ERROR, None));
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ListServices { instance, json, db } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({});
                if let Some(ref inst) = instance {
                    args["instance"] = serde_json::json!(inst);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "list_services", args)
                {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        let services: Vec<nestweaver_schema::Service> =
                            serde_json::from_value(value).unwrap_or_default();
                        if services.is_empty() {
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
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({ "name": name });
                if let Some(ref inst) = instance {
                    args["instance"] = serde_json::json!(inst);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "service_summary", args)
                {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        let s: nestweaver_schema::Service = serde_json::from_value(value)
                            .unwrap_or_else(|_| nestweaver_schema::Service {
                                uid: String::new(),
                                name: name.clone(),
                                repo_uid: String::new(),
                                summary: None,
                                summary_hash: None,
                                embedding: None,
                            });
                        println!("Service: {}", s.name);
                        if let Some(ref summary) = s.summary {
                            println!("Summary: {summary}");
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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
            // ── daemon guard (JSON pass-through) ─────────────────
            if json && use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({ "token_budget": token_budget });
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "repo_map", args) {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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
            repo: repo_filter,
            json,
            db,
        } => {
            // ── daemon guard (JSON pass-through) ─────────────────
            if json && use_daemon {
                let db_default = default_db_path();
                let db_path = db.as_deref().unwrap_or(&db_default);
                let mut args = serde_json::json!({ "name_or_uid": name_or_uid });
                if let Some(ref rf) = repo_filter {
                    args["repo"] = serde_json::json!(rf);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, db_path, None, "cross_repo_contracts", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(db.as_deref())?;
            match resolve_uid_with_repo_filter(&store, &name_or_uid, repo_filter.as_deref())? {
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
                            || nestweaver_engine::repo_display_name(r)
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
                        || nestweaver_engine::repo_display_name(r)
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
            limit,
            token_budget,
            json,
            db,
        } => {
            // ── daemon guard (typed GetContext RPC) ───────────────
            // Route through the daemon when JSON output is requested and
            // we're in normal seed-based mode (not --feature, which
            // requires config-file processing the daemon doesn't handle
            // for this legacy command).
            if json && feature.is_none() && use_daemon {
                let db_default = default_db_path();
                let db_path = db.as_deref().unwrap_or(&db_default);
                if let Ok(rt) = tokio::runtime::Runtime::new() {
                    let connect = rt.block_on(nestweaver_client::DaemonClient::connect(
                        db_path,
                        config.as_deref(),
                    ));
                    if let Ok(mut client) = connect {
                        let req = nestweaver_proto::BrainContextRequest {
                            seeds: seeds.clone(),
                            token_budget: token_budget.unwrap_or(0) as i32,
                            response_format: String::new(),
                            repos: vec![],
                            vaults: vec![],
                            kinds: vec![],
                            path_prefix: String::new(),
                            tags: vec![],
                            exclude_tags: vec![],
                            weight_ppr: 0.0,
                            weight_bm25: 0.0,
                            intent: intent.clone().unwrap_or_default(),
                            include_seeds: true,
                            include_bodies: false,
                            root: String::new(),
                            prf: false,
                            rerank: false,
                            weight_semantic: 0.0,
                            since: String::new(),
                            recency_weight: 0.0,
                            recency_half_life_days: 0.0,
                        };
                        let rpc = rt.block_on(async {
                            client
                                .inner_mut()
                                .get_context(req)
                                .await
                                .map(|r| r.into_inner())
                        });
                        if let Ok(resp) = rpc {
                            let result: nestweaver_engine::BrainContextResult =
                                serde_json::from_str(&resp.result_json)?;
                            let cut = match token_budget {
                                Some(budget) => token_budgeted_truncate(&result.connected, budget),
                                None => limit.unwrap_or(30).min(result.connected.len()),
                            };
                            print_brain_context_json(&result, cut)?;
                            let stats = format!(
                                "{} seeds, {} connected nodes in {} (via daemon)",
                                result.seeds.len(),
                                cut,
                                format_elapsed(t0.elapsed())
                            );
                            return Ok((EXIT_SUCCESS, Some(stats)));
                        }
                    }
                }
            }

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

                match build_feature_context(
                    &store,
                    feature_config,
                    links,
                    &instance_config.repos,
                    parsed_intent,
                    limit,
                ) {
                    Ok(mut result) => {
                        if let Some(budget) = token_budget {
                            let cut = context_token_budgeted_truncate(&result.connected, budget);
                            result.connected.truncate(cut);
                        }
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
                match build_context_with_intent(&store, &seeds, parsed_intent, limit) {
                    Ok(mut result) => {
                        if let Some(budget) = token_budget {
                            let cut = context_token_budgeted_truncate(&result.connected, budget);
                            result.connected.truncate(cut);
                        }
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

        Commands::ListLinks {
            config,
            json,
            db: _,
        } => {
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
            let config_path =
                config.ok_or_else(|| anyhow::anyhow!("--config is required for list-features"))?;
            let instance_config = nestweaver_engine::InstanceConfig::from_file(&config_path)?;
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

        Commands::SuggestLinks {
            db,
            json,
            config: _,
        } => {
            // ── daemon guard (JSON pass-through) ─────────────────
            if json && use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({});
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "suggest_links", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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
            rules_from,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard (JSON pass-through) ─────────────────
            // When output is stdout (no --output file) and no --rules-from
            // override, try the daemon first — it can generate the guide
            // without the CLI opening the DB directly.
            if output.is_none() && rules_from.is_none() && use_daemon {
                let mut args = serde_json::json!({ "format": format });
                if let Some(ref c) = config {
                    args["config"] = serde_json::json!(c.to_string_lossy());
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, config.as_deref(), "brain_guide", args)
                {
                    // brain_guide returns the guide text as a JSON string.
                    if let Some(text) = value.as_str() {
                        print!("{text}");
                    } else {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let instance_config = config
                .as_deref()
                .map(nestweaver_engine::InstanceConfig::from_file)
                .transpose()?;
            // Optional hard-rule override loaded from a TOML/markdown file.
            let override_rules = match &rules_from {
                Some(path) => {
                    let contents = std::fs::read_to_string(path)?;
                    Some(nestweaver_engine::parse_rules_override(&contents)?)
                }
                None => None,
            };
            let rules_ref = override_rules.as_deref();
            let cfg_ref = instance_config.as_ref();
            // Build tool docs from the live MCP registry for dynamic guide generation
            let tool_docs: Vec<nestweaver_engine::ToolDocEntry> =
                nestweaver_mcp::tools::tool_doc_entries()
                    .into_iter()
                    .map(
                        |(name, category, purpose, params)| nestweaver_engine::ToolDocEntry {
                            name,
                            category,
                            purpose,
                            key_params: params,
                        },
                    )
                    .collect();

            let output_str = match format.as_str() {
                "skill" => nestweaver_engine::generate_skill_with_tools(
                    &store, cfg_ref, rules_ref, &tool_docs,
                )?,
                "cursor-rule" => generate_cursor_rule_with_rules(&store, cfg_ref, rules_ref)?,
                "agents-md" => generate_agents_md_with_rules(&store, cfg_ref, rules_ref, None)?,
                "claude-md" => generate_claude_md_with_rules(&store, cfg_ref, rules_ref)?,
                _ => generate_guide_with_rules(&store, cfg_ref, rules_ref)?,
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

        Commands::Admin { command } => match command {
            AdminCommands::Instructions {
                for_subagent,
                set,
                reset,
            } => {
                use nestweaver_engine::admin;
                if reset {
                    admin::reset_instructions()?;
                    out.status("Instruction stores reset to bundled defaults.");
                    return Ok((EXIT_SUCCESS, None));
                }
                if let Some(src) = set {
                    let dst = if for_subagent {
                        admin::set_subagent_instructions(&src)?
                    } else {
                        admin::set_main_instructions(&src)?
                    };
                    out.status(&format!("Installed instructions to {}", dst.display()));
                    return Ok((EXIT_SUCCESS, None));
                }
                // No flag (or only --for-subagent): print the relevant store.
                // --for-subagent prints a single clean stdout payload for hooks.
                let text = if for_subagent {
                    admin::read_subagent_instructions()?
                } else {
                    admin::read_main_instructions()?
                };
                print!("{text}");
                Ok((EXIT_SUCCESS, None))
            }
            AdminCommands::InstallHook { runtime, dry_run } => {
                use nestweaver_engine::admin;
                let rt = admin::Runtime::parse(&runtime)?;
                let settings_path = admin::runtime_settings_path(rt);
                let existing: serde_json::Value = if settings_path.exists() {
                    let raw = std::fs::read_to_string(&settings_path)?;
                    serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::Value::Null
                };
                if dry_run {
                    // PRINT only the minimal delta that WOULD be added — not the
                    // whole merged settings document (which may contain unrelated
                    // pre-existing permissions). Do not write.
                    let delta = admin::compute_hook_delta(rt, &existing)?;
                    println!("{}", serde_json::to_string_pretty(&delta)?);
                    eprintln!(
                        "(dry-run) Would merge the above hook entry into {} (existing settings preserved). Injected guidance helps but is NOT enforcement (Geng et al. 2025); hook schema is Claude-Code-specific.",
                        settings_path.display()
                    );
                    return Ok((EXIT_SUCCESS, None));
                }
                let patched = admin::compute_hook_patch(rt, &existing)?;
                if let Some(parent) = settings_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&settings_path, serde_json::to_string_pretty(&patched)?)?;
                out.status(&format!(
                    "Hook installed (idempotent) to {}",
                    settings_path.display()
                ));
                Ok((EXIT_SUCCESS, None))
            }
        },

        Commands::Hubs {
            top,
            json,
            db,
            config: _,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard (typed RPC) ──────────────────────────
            if use_daemon && let Ok(rt) = tokio::runtime::Runtime::new() {
                let connect = rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None));
                if let Ok(mut client) = connect {
                    let req = nestweaver_proto::HubNodesRequest {
                        top_n: top as i32,
                        response_format: String::new(),
                    };
                    let rpc = rt.block_on(async {
                        client
                            .inner_mut()
                            .hub_nodes(req)
                            .await
                            .map(|r| r.into_inner())
                    });
                    match rpc {
                        Ok(resp) => {
                            let value: serde_json::Value = serde_json::from_str(&resp.result_json)
                                .unwrap_or(serde_json::json!({}));
                            if json {
                                println!("{}", serde_json::to_string_pretty(&value)?);
                            } else {
                                let hubs: Vec<HubNode> = value
                                    .get("hubs")
                                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                                    .unwrap_or_default();
                                if hubs.is_empty() {
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
                            }
                            let stats = format!(
                                "{} hubs in {} (via daemon)",
                                value.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
                                format_elapsed(t0.elapsed())
                            );
                            return Ok((EXIT_SUCCESS, Some(stats)));
                        }
                        Err(status) => {
                            eprintln!(
                                "warning: daemon hub_nodes RPC failed ({}); falling back to direct DB read",
                                status.message()
                            );
                        }
                    }
                }
            }

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

        Commands::Bridges {
            top,
            json,
            db,
            config: _,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let args = serde_json::json!({ "top": top });
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "bridge_nodes", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({ "level": level });
                if let Some(tb) = token_budget {
                    args["token_budget"] = serde_json::json!(tb);
                }
                if let Some(ref t) = target {
                    args["target"] = serde_json::json!(t);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "get_summary", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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
            config: _,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({});
                if let Some(r) = resolution {
                    args["resolution"] = serde_json::json!(r);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "clusters", args) {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            // Compute and save inside a block so the store is dropped
            // before any output. LadybugDB's connection finaliser can
            // trigger a panic during WAL checkpoint; wrapping in
            // catch_unwind prevents the Drop panic from aborting the
            // process (exit code 101).
            let output = {
                let store = std::mem::ManuallyDrop::new(open_store(Some(&db_path))?);

                // Adaptive resolution: pick a sensible default based on
                // graph size.  Large graphs (>10 K symbols) benefit from
                // lower resolution to avoid the explosion of tiny
                // communities that resolution=1.0 produces.
                let sym_count = store.count_symbols().unwrap_or(0);
                let effective_resolution =
                    resolution.unwrap_or(if sym_count > 10_000 { 0.3 } else { 0.5 });

                out.status(&format!(
                    "Computing clusters (resolution={effective_resolution}, symbols={sym_count})..."
                ));
                let o = compute_clusters(&store, effective_resolution)?;
                save_clusters(&db_path, &o)?;
                out.status(&format!(
                    "Found {} community(ies), modularity={:.4}. Saved to sidecar.",
                    o.communities.len(),
                    o.modularity
                ));
                // Leak the store intentionally — LadybugDB's Drop can
                // panic during WAL checkpoint on some platforms, and we
                // are about to exit anyway.  process::exit (called by
                // main) terminates without running destructors, so this
                // is safe.
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

            // ── daemon guard (JSON pass-through) ─────────────────
            if json && use_daemon {
                let args = serde_json::json!({ "id_or_name": id_or_name });
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "clusters", args) {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            // Load cached clusters from sidecar. If none exist, compute them.
            let output = match load_clusters(&db_path)? {
                Some(cached) => cached,
                None => {
                    out.status("No cached clusters found; computing with default resolution...");
                    let store = open_store(Some(&db_path))?;
                    let sym_count = store.count_symbols().unwrap_or(0);
                    let default_res = if sym_count > 10_000 { 0.3 } else { 0.5 };
                    let computed = compute_clusters(&store, default_res)?;
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
            force,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            setup::run_setup(tool.as_deref(), &db_path, all, allow_writes, force)?;
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Interactions { command } => match command {
            InteractionCommands::Status { db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                match nestweaver_engine::load_interaction_data(&db_path) {
                    Some(data) => {
                        let node_count = data.scores.len();
                        let event_count = data.event_count;
                        println!("Interaction memory:");
                        println!("  Nodes with scores: {node_count}");
                        println!("  Total events:      {event_count}");
                        if let Some(ts) = data.oldest_timestamp {
                            println!("  Oldest event:      {}", format_epoch_timestamp(ts));
                        }
                        if let Some(ts) = data.newest_timestamp {
                            println!("  Newest event:      {}", format_epoch_timestamp(ts));
                        }
                    }
                    None => {
                        println!("No interaction data found.");
                        println!("Use `nestweaver mcp --track-interactions` to start recording.");
                    }
                }
                Ok((EXIT_SUCCESS, None))
            }
            InteractionCommands::Clear { db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                if nestweaver_engine::clear_interaction_sidecar(&db_path) {
                    println!("Interaction memory cleared.");
                } else {
                    println!("No interaction data to clear.");
                }
                Ok((EXIT_SUCCESS, None))
            }
            InteractionCommands::Show { uid, top, kind, db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                if let Some(n) = top {
                    let rows = nestweaver_engine::top_uids_by_kind(&db_path, &kind, n);
                    if rows.is_empty() {
                        println!("No interaction data for kind '{kind}'.");
                    } else {
                        println!("Top {} UIDs by {kind}:", rows.len());
                        for (uid, value) in rows {
                            println!("  {value:>10.4}  {uid}");
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                } else if let Some(uid) = uid {
                    match nestweaver_engine::load_node_score(&db_path, &uid) {
                        Some(ns) => {
                            println!("Interaction record for {uid}:");
                            println!("  query_seed_count:       {}", ns.query_seed_count);
                            println!("  access_count:           {}", ns.access_count);
                            println!("  result_shown_count:     {}", ns.result_shown_count);
                            println!("  result_used_count:      {}", ns.result_used_count);
                            println!("  terminal_success_count: {}", ns.terminal_success_count);
                            println!("  distinct_sessions:      {}", ns.distinct_sessions);
                            if ns.last_accessed > 0.0 {
                                println!(
                                    "  last_accessed:          {}",
                                    format_epoch_timestamp(ns.last_accessed)
                                );
                            }
                            println!("  decayed_score:          {:.4}", ns.computed_score);
                            Ok((EXIT_SUCCESS, None))
                        }
                        None => {
                            println!("No interaction data for UID '{uid}'.");
                            Ok((EXIT_NOT_FOUND, None))
                        }
                    }
                } else {
                    eprintln!("Provide --uid <uid> or --top <N> [--kind <kind>].");
                    Ok((EXIT_ERROR, None))
                }
            }
        },

        Commands::Rerank { command } => match command {
            RerankCommands::ExportTraining { out, db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let out_path = out.unwrap_or_else(|| {
                    nestweaver_engine::sidecar_path(&db_path, ".rerank-training.jsonl")
                });
                let rows = nestweaver_engine::export_training_rows(&db_path, &out_path)?;
                println!(
                    "Exported {rows} training row(s) to {} (SCAFFOLD — no model is trained here).",
                    out_path.display()
                );
                if rows == 0 {
                    println!(
                        "No interaction data found. Enable `nestweaver mcp --track-interactions` to accumulate labels."
                    );
                }
                println!(
                    "Note: a learned reranker is only trustworthy after the eval harness gates it at >= 5% nDCG@10; the default scorer is a transparent heuristic."
                );
                Ok((EXIT_SUCCESS, None))
            }
        },

        Commands::Contracts { command } => run_contracts(command),

        Commands::Snapshot { command } => run_snapshot(command).map(|c| (c, None)),
        Commands::Instance { command } => run_instance(command).map(|c| (c, None)),
        Commands::Brain { command } => run_brain(*command, out, t0, use_daemon),
        Commands::Memory { command } => run_memory(*command, t0),
        Commands::Ranking { command } => run_ranking(command, t0),
        Commands::Eval { command } => run_eval_cmd(command).map(|c| (c, None)),
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
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({ "min_confidence": min_confidence });
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "dead_code", args) {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let min_conf =
                DeadCodeConfidence::from_str_loose(&min_confidence).unwrap_or_else(|| {
                    eprintln!(
                        "Warning: unknown confidence level '{}', defaulting to 'low'",
                        min_confidence
                    );
                    DeadCodeConfidence::Low
                });
            let store = open_store(db.as_deref())?;

            // Load manifest sidecar for manifest-driven entry points.
            let db_path = db.clone().unwrap_or_else(default_db_path);
            let manifest_cache_path = nestweaver_engine::sidecar_path(&db_path, ".manifests.json");
            let manifests =
                nestweaver_engine::load_manifest_cache(&manifest_cache_path).unwrap_or_default();

            let result = nestweaver_engine::detect_dead_code_with_manifests(&store, &manifests)?;

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
                    excluded_count: usize,
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
                        excluded_count: result.excluded_count,
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
                if result.excluded_count > 0 {
                    println!(
                        "({} type-only/declaration symbols excluded from analysis)",
                        result.excluded_count
                    );
                }
            } else {
                println!(
                    "Dead code analysis: {} of {} symbols ({:.1}%) unreachable from entry points\n",
                    result.unreachable_symbols.len(),
                    result.total_symbols,
                    result.dead_percentage,
                );
                if result.excluded_count > 0 {
                    println!(
                        "({} type-only/declaration symbols excluded from analysis)",
                        result.excluded_count
                    );
                }
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
            let db_default = default_db_path();
            let db_path = db.as_deref().unwrap_or(&db_default);

            // Stop the daemon so we can open the store directly for export.
            let daemon_was_running = if use_daemon {
                stop_daemon_if_running(db_path)
            } else {
                false
            };

            let store = open_store(db.as_deref())?;

            if format == "msgpack" {
                let graph = export_in_memory_graph(&store)?;
                let bytes = rmp_serde::to_vec(&graph)
                    .with_context(|| "failed to serialize graph to msgpack")?;
                let path = match &output {
                    Some(p) => p.clone(),
                    None => {
                        let mut name = db_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        name.push_str(".graph.msgpack");
                        db_path
                            .parent()
                            .unwrap_or(std::path::Path::new("."))
                            .join(name)
                    }
                };
                std::fs::write(&path, &bytes)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                out.status(&format!(
                    "Exported graph to {} ({} nodes, {} edges, {} bytes)",
                    path.display(),
                    graph.uids.len(),
                    graph.edges.len(),
                    bytes.len()
                ));
                let stats = format!("exported msgpack in {}", format_elapsed(t0.elapsed()));
                drop(store);
                if daemon_was_running {
                    restart_daemon(db_path, None);
                }
                return Ok((EXIT_SUCCESS, Some(stats)));
            }

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
                        "Unknown format '{}'. Supported: cypher, graphml, mermaid, msgpack",
                        other
                    );
                    drop(store);
                    if daemon_was_running {
                        restart_daemon(db_path, None);
                    }
                    return Ok((EXIT_ERROR, None));
                }
            }

            if let Some(path) = &output {
                out.status(&format!("Exported graph to {}", path.display()));
            }

            drop(store);
            if daemon_was_running {
                restart_daemon(db_path, None);
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

            // ── daemon guard (JSON pass-through) ─────────────────
            if json && use_daemon {
                let file_strs: Vec<&str> =
                    changed_files.iter().filter_map(|p| p.to_str()).collect();
                let args = serde_json::json!({
                    "files": file_strs,
                    "depth": depth,
                });
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "pr_impact", args) {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;

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

        Commands::AffectedTests {
            files,
            base_ref,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({});
                if let Some(ref f) = files {
                    args["files"] = serde_json::json!(f);
                }
                if let Some(ref br) = base_ref {
                    args["base_ref"] = serde_json::json!(br);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "affected_tests", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;

            // Resolve changed files: explicit --files, else git diff against --base-ref.
            let changed_files: Vec<String> = if let Some(files_str) = files {
                files_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else if let Some(base) = base_ref {
                let repo_root = detect_repo_root();
                out.status(&format!("Detecting changed files via git diff {base}..."));
                changed_files_from_git(&repo_root, Some(&base))
                    .context("git diff")?
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            } else {
                eprintln!("Error: provide either --files or --base-ref");
                return Ok((EXIT_ERROR, None));
            };

            if changed_files.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "changed_files": [],
                            "changed_symbols": [],
                            "tier_1": [],
                            "tier_2": [],
                            "tier_3": [],
                            "summary": "0 tier-1, 0 tier-2, 0 tier-3 tests affected",
                        }))?
                    );
                } else {
                    println!("No changed files detected.");
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let result = affected_tests(&store, &changed_files)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.summary);
                println!();
                if !result.changed_symbols.is_empty() {
                    println!("Changed symbols ({}):", result.changed_symbols.len());
                    for s in &result.changed_symbols {
                        println!("  {} — {}", s.name, s.file_path);
                    }
                    println!();
                }
                let print_tier = |label: &str, tier: &[nestweaver_engine::AffectedTestFile]| {
                    if tier.is_empty() {
                        return;
                    }
                    println!("{label} ({} file(s)):", tier.len());
                    for f in tier {
                        println!(
                            "  {} (conf {:.2}) — {}",
                            f.test_file,
                            f.confidence,
                            f.tests.join(", ")
                        );
                    }
                    println!();
                };
                print_tier("Tier 1 (direct)", &result.tier_1);
                print_tier("Tier 2 (caller's tests)", &result.tier_2);
                print_tier("Tier 3 (transitive)", &result.tier_3);
                println!("Note: {}", result.disclaimer);
            }

            let stats = format!("{} in {}", result.summary, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Watch {
            repo,
            db,
            instance,
            refresh_wiki_hours,
            config,
        } => {
            if refresh_wiki_hours.is_some() && config.is_none() {
                eprintln!("Error: --refresh-wiki-hours requires --config");
                return Ok((EXIT_ERROR, None));
            }
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

            if let Some(hours) = refresh_wiki_hours {
                eprintln!(
                    "Wiki refresh scheduled every {}h (via materialize-instance)",
                    hours
                );
            }

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

            // Spawn periodic wiki refresh thread if --refresh-wiki-hours
            // is set. Same pattern as the brain watch handler.
            if let (Some(hours), Some(config_path)) = (refresh_wiki_hours, config.as_deref()) {
                let wiki_db = db_path.clone();
                let wiki_config_path = config_path.to_path_buf();
                let wiki_stop = stop.clone();
                let wiki_instance = instance_id.clone();
                std::thread::spawn(move || {
                    let interval = std::time::Duration::from_secs(hours * 3600);
                    loop {
                        let deadline = std::time::Instant::now() + interval;
                        while std::time::Instant::now() < deadline {
                            if wiki_stop.is_stopped() {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(5));
                        }
                        if wiki_stop.is_stopped() {
                            return;
                        }
                        tracing::info!("periodic wiki refresh triggered");
                        match nestweaver_engine::InstanceConfig::from_file(&wiki_config_path) {
                            Ok(cfg) => {
                                let store = match GraphStore::open_or_create(&wiki_db) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        tracing::warn!("wiki refresh: failed to open store: {e}");
                                        continue;
                                    }
                                };
                                match materialize_projects(&store, &cfg, &wiki_instance, &wiki_db) {
                                    Ok(res) => tracing::info!(
                                        projects = res.projects_created,
                                        wiki_notes = res.wiki_notes_ingested,
                                        "wiki refresh complete"
                                    ),
                                    Err(e) => tracing::warn!("wiki refresh failed: {e}"),
                                }
                            }
                            Err(e) => tracing::warn!("wiki refresh: config load failed: {e}"),
                        }
                    }
                });
            }

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
            tools: tool_allowlist,
            track_interactions,
            config,
            no_daemon,
        } => {
            if allow_mcp_add_sources {
                eprintln!(
                    "warning: --allow-mcp-add-sources is deprecated and will be removed in a future release; \
                     remove it from your MCP config"
                );
            }
            let db_path = db.unwrap_or_else(default_db_path);
            if let Some(ref allowed) = tool_allowlist {
                nestweaver_mcp::tools::set_allowed_tools(allowed.clone());
            }
            if track_interactions {
                nestweaver_mcp::tools::set_track_interactions(true);
            }
            let use_daemon_mcp = !no_daemon && std::env::var("NESTWEAVER_NO_DAEMON").is_err();
            if use_daemon_mcp {
                let rt = tokio::runtime::Runtime::new()
                    .context("create tokio runtime for daemon proxy")?;
                let daemon_client = rt
                    .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                    .context("connect to daemon")?;
                let grpc_client = daemon_client.into_inner();
                nestweaver_mcp::run_stdio_server_daemon(
                    grpc_client,
                    rt,
                    lite,
                    track_interactions,
                    &db_path,
                )
                .context("mcp server (daemon mode)")?;
            } else {
                nestweaver_mcp::run_stdio_server(
                    &db_path,
                    allow_mcp_add_sources,
                    lite,
                    track_interactions,
                    config.as_deref(),
                )
                .context("mcp server")?;
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::Ui {
            db,
            port,
            config,
            no_open,
            watch,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // Stop the daemon so we can open the store directly for the UI server.
            let daemon_was_running = if use_daemon {
                stop_daemon_if_running(&db_path)
            } else {
                false
            };

            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            let state = if watch {
                // Watch mode needs write access for the CodeWatcher.
                let store =
                    std::sync::Arc::new(GraphStore::open_or_create(&db_path).with_context(
                        || format!("failed to open database at {}", db_path.display()),
                    )?);
                nestweaver_web::state::AppState::new_with_store(store, tantivy, db_path.clone())
            } else {
                let store = open_store(Some(&db_path))?;
                nestweaver_web::state::AppState::new(store, tantivy, db_path.clone())
            };

            if watch {
                let repo_root = detect_repo_root();
                let code_store = state.store.clone();
                let code_tx = state.event_tx.clone();
                let code_db = db_path.clone();
                let code_instance = "default".to_string();

                std::thread::spawn(move || {
                    let watcher = CodeWatcher::new(&code_db, &repo_root, &code_instance);
                    let store_for_cb = code_store.clone();
                    let on_change = Box::new(move || {
                        let generation = store_for_cb.graph_generation();
                        let _ = code_tx.send(nestweaver_web::state::GraphEvent {
                            event_type: "graph:updated".to_string(),
                            payload: serde_json::json!({"source": "code_watcher", "generation": generation}),
                        });
                    });
                    if let Err(e) = watcher.run_with_store(code_store, Some(on_change)) {
                        tracing::error!("CodeWatcher failed: {e}");
                    }
                });
            }

            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(nestweaver_web::start_server(state, port, !no_open))?;

            // Restart daemon if we stopped it.
            if daemon_was_running {
                restart_daemon(&db_path, config.as_deref().map(std::path::Path::new));
            }

            Ok((EXIT_SUCCESS, None))
        }

        Commands::Search {
            query,
            limit,
            json,
            db,
            config,
        } => {
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 10);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({ "query": query, "limit": limit });
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "search_symbols", args)
                {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        let candidates: Vec<nestweaver_engine::SymbolCandidate> =
                            serde_json::from_value(value).unwrap_or_default();
                        if candidates.is_empty() {
                            println!("No symbols found matching '{query}'.");
                        } else {
                            println!("Found {} symbol(s) matching '{query}':", candidates.len());
                            for c in &candidates {
                                println!(
                                    "  {} ({}) {}:{}",
                                    c.name, c.kind, c.file_path, c.start_line
                                );
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

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

        Commands::RegexSearch {
            pattern,
            path_prefix,
            kinds,
            limit,
            max_millis,
            json,
            db,
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({ "pattern": pattern });
                if let Some(ref pp) = path_prefix {
                    args["path_prefix"] = serde_json::json!(pp);
                }
                if let Some(ref k) = kinds {
                    args["kinds"] = serde_json::json!(k);
                }
                if let Some(l) = limit {
                    args["limit"] = serde_json::json!(l);
                }
                if let Some(ms) = max_millis {
                    args["max_millis"] = serde_json::json!(ms);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "regex_search", args)
                {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        let res: nestweaver_store::regex::RegexSearchResult =
                            serde_json::from_value(value).unwrap_or_else(|_| {
                                nestweaver_store::regex::RegexSearchResult {
                                    results: vec![],
                                    truncated: false,
                                    scanned_fallback: false,
                                }
                            });
                        if res.results.is_empty() {
                            println!("No matches for '{pattern}'.");
                        } else {
                            println!("Found {} match(es) for '{pattern}':", res.results.len());
                            for m in &res.results {
                                println!(
                                    "  [{}] {} {} — {}",
                                    m.kind, m.title, m.location, m.snippet
                                );
                            }
                            if res.truncated {
                                println!("(results truncated — hit candidate cap or time budget)");
                            }
                            if res.scanned_fallback {
                                println!(
                                    "(no trigram pre-filter used — run `index --with-trigrams` for speed)"
                                );
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(db.as_deref())?;
            let res = store
                .regex_search(
                    &pattern,
                    path_prefix.as_deref(),
                    kinds.as_deref(),
                    limit,
                    max_millis,
                )
                .context("regex_search")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if res.results.is_empty() {
                println!("No matches for '{pattern}'.");
            } else {
                println!("Found {} match(es) for '{pattern}':", res.results.len());
                for m in &res.results {
                    println!("  [{}] {} {} — {}", m.kind, m.title, m.location, m.snippet);
                }
                if res.truncated {
                    println!("(results truncated — hit candidate cap or time budget)");
                }
                if res.scanned_fallback {
                    println!(
                        "(no trigram pre-filter used — run `index --with-trigrams` for speed)"
                    );
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::CountPatterns {
            patterns,
            path_prefix,
            kinds,
            json,
            db,
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({ "patterns": patterns });
                if let Some(ref pp) = path_prefix {
                    args["path_prefix"] = serde_json::json!(pp);
                }
                if let Some(ref k) = kinds {
                    args["kinds"] = serde_json::json!(k);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "count_patterns", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(db.as_deref())?;
            let counts = store
                .count_patterns(&patterns, path_prefix.as_deref(), kinds.as_deref())
                .context("count_patterns")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&counts)?);
            } else {
                for c in &counts {
                    println!(
                        "'{}': {} match(es) across {} file(s)",
                        c.pattern, c.total_matches, c.files_matched
                    );
                    for f in &c.top_files {
                        println!("    {} ({})", f.path, f.count);
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::ReadSymbols {
            targets,
            neighbors,
            token_budget,
            root,
            json,
            db,
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let mut args = serde_json::json!({
                    "targets": targets,
                    "neighbors": neighbors,
                    "token_budget": token_budget,
                });
                if let Some(ref r) = root {
                    args["root"] = serde_json::json!(r);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "read_symbols", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(db.as_deref())?;
            let root = root.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let res = nestweaver_engine::read_symbols::read_symbols(
                &store,
                &targets,
                &root,
                neighbors,
                token_budget,
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                for w in &res.symbols {
                    let tag = if w.is_neighbor { " [neighbor]" } else { "" };
                    println!(
                        "\u{2500}\u{2500} {} ({}) {}:{}-{}{}",
                        w.name, w.kind, w.path, w.start_line, w.end_line, tag
                    );
                    println!("{}", w.body);
                    println!();
                }
                for nf in &res.not_found {
                    eprintln!("not found: {nf}");
                }
                for a in &res.ambiguous {
                    eprintln!(
                        "ambiguous: {} \u{2192} {} candidates (pass a UID)",
                        a.query,
                        a.candidate_uids.len()
                    );
                }
                if res.truncated {
                    eprintln!(
                        "truncated: {} symbol(s) dropped for token budget",
                        res.dropped.len()
                    );
                }
            }
            // Exit 2 when targets were requested but none resolved to a symbol
            // (consistent with `symbol`/`impact`). When at least one target
            // resolves, succeed even if others were not-found/ambiguous.
            if !targets.is_empty() && res.symbols.is_empty() {
                return Ok((EXIT_NOT_FOUND, None));
            }
            Ok((EXIT_SUCCESS, None))
        }
        Commands::Symbol {
            name_or_uid,
            json,
            db,
            ..
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({ "name_or_uid": name_or_uid });
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "symbol_lookup", args)
                {
                    let status = value
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("not_found");
                    match status {
                        "found" => {
                            if json {
                                if let Some(detail) = value.get("detail") {
                                    println!("{}", serde_json::to_string_pretty(detail)?);
                                }
                            } else if let Some(detail) = value.get("detail") {
                                let d: nestweaver_engine::SymbolDetail =
                                    serde_json::from_value(detail.clone())
                                        .map_err(|e| anyhow::anyhow!("deserialize: {e}"))?;
                                let s = &d.symbol;
                                if out.verbose {
                                    println!("Symbol: {} [{}]", s.name, s.uid);
                                } else {
                                    println!("Symbol: {}", s.name);
                                }
                                println!("Kind: {}", s.kind);
                                println!("File: {}:{}", s.file_path, s.start_line);
                                println!("Signature: {}", s.signature);
                                if !d.callers.is_empty() {
                                    if !out.quiet {
                                        println!("\nCallers ({}):", d.callers.len());
                                    }
                                    for c in &d.callers {
                                        if out.verbose {
                                            println!(
                                                "  {} ({}:{}) [{}]",
                                                c.name, c.file_path, c.start_line, c.uid
                                            );
                                        } else {
                                            println!(
                                                "  {} ({}:{})",
                                                c.name, c.file_path, c.start_line
                                            );
                                        }
                                    }
                                }
                                if !d.callees.is_empty() {
                                    if !out.quiet {
                                        println!("\nCallees ({}):", d.callees.len());
                                    }
                                    for c in &d.callees {
                                        if out.verbose {
                                            println!(
                                                "  {} ({}:{}) [{}]",
                                                c.name, c.file_path, c.start_line, c.uid
                                            );
                                        } else {
                                            println!(
                                                "  {} ({}:{})",
                                                c.name, c.file_path, c.start_line
                                            );
                                        }
                                    }
                                }
                            }
                            return Ok((EXIT_SUCCESS, None));
                        }
                        "ambiguous" => {
                            if json {
                                if let Some(candidates) = value.get("candidates") {
                                    println!("{}", serde_json::to_string_pretty(candidates)?);
                                }
                            } else {
                                let candidates: Vec<nestweaver_engine::SymbolCandidate> = value
                                    .get("candidates")
                                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                                    .unwrap_or_default();
                                eprintln!(
                                    "Ambiguous: '{}' matches {} symbols:",
                                    name_or_uid,
                                    candidates.len()
                                );
                                for c in &candidates {
                                    eprintln!(
                                        "  {} [{}] {}:{}",
                                        c.uid, c.kind, c.file_path, c.start_line
                                    );
                                }
                            }
                            return Ok((EXIT_AMBIGUOUS, None));
                        }
                        _ => {
                            // not_found
                            if json {
                                println!(
                                    "{}",
                                    serde_json::json!({"error": "not found", "name": name_or_uid})
                                );
                            } else {
                                eprintln!("Symbol '{name_or_uid}' not found.");
                            }
                            return Ok((EXIT_NOT_FOUND, None));
                        }
                    }
                }
            }

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
            repo: repo_filter,
            config: _,
            ..
        } => {
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                if let Some(value) = try_daemon_json_rpc(
                    true,
                    &db_path,
                    None,
                    "brain_impact",
                    serde_json::json!({
                        "symbol": name_or_uid,
                        "depth": depth,
                        "min_confidence": confidence,
                    }),
                ) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else if let Some(arr) = value.get("impact_nodes") {
                        #[derive(serde::Deserialize)]
                        struct DaemonImpactNode {
                            uid: String,
                            name: String,
                            file_path: String,
                            start_line: u32,
                            edge_type: String,
                            confidence: f32,
                            depth: u32,
                        }
                        let nodes: Vec<DaemonImpactNode> =
                            serde_json::from_value(arr.clone()).unwrap_or_default();
                        let count = nodes.len();
                        if nodes.is_empty() {
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
                            "{} affected symbols in {} (via daemon)",
                            count,
                            format_elapsed(t0.elapsed())
                        );
                        return Ok((EXIT_SUCCESS, Some(stats)));
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(db.as_deref())?;

            // Resolve the symbol UID first (may be a name).
            match resolve_uid_with_repo_filter(&store, &name_or_uid, repo_filter.as_deref())? {
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

        Commands::ListProjects { json, db, config } => {
            // ── daemon guard ──────────────────────────────────────
            let materialized: Vec<nestweaver_schema::Project> = if use_daemon {
                let db_path = db.clone().unwrap_or_else(default_db_path);
                let args = serde_json::json!({});
                try_daemon_json_rpc(true, &db_path, None, "list_projects", args)
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_else(|| {
                        let store = open_store(db.as_deref()).expect("open_store");
                        store.list_projects().unwrap_or_default()
                    })
            } else {
                let store = open_store(db.as_deref())?;
                store.list_projects().map_err(|e| anyhow::anyhow!(e))?
            };

            // When --config is provided, also surface declared projects from
            // [[projects]] that haven't been materialized into the store yet.
            let declared_only: Vec<nestweaver_engine::ProjectConfig> =
                if let Some(ref cfg_path) = config {
                    let instance_config = nestweaver_engine::InstanceConfig::from_file(cfg_path)?;
                    instance_config
                        .projects
                        .into_iter()
                        .filter(|pc| !materialized.iter().any(|m| m.name == pc.name))
                        .collect()
                } else {
                    Vec::new()
                };

            if json {
                #[derive(serde::Serialize)]
                struct ListProjectsJson<'a> {
                    materialized: &'a [nestweaver_schema::Project],
                    #[serde(
                        skip_serializing_if = "<[nestweaver_engine::ProjectConfig]>::is_empty"
                    )]
                    declared: &'a [nestweaver_engine::ProjectConfig],
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ListProjectsJson {
                        materialized: &materialized,
                        declared: &declared_only,
                    })?
                );
            } else if materialized.is_empty() && declared_only.is_empty() {
                println!(
                    "No projects found. Use an instance config with [[projects]] to define them."
                );
            } else {
                if !materialized.is_empty() {
                    for p in &materialized {
                        println!("{}", p.name);
                        println!("  UID:      {}", p.uid);
                        println!("  Instance: {}", p.instance_id);
                        if let Some(ref summary) = p.summary {
                            println!("  Summary:  {summary}");
                        }
                        println!();
                    }
                }
                if !declared_only.is_empty() {
                    println!("Declared in config (not yet materialized):");
                    for pc in &declared_only {
                        println!("  {}", pc.name);
                        if let Some(ref desc) = pc.description {
                            println!("    {desc}");
                        }
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
            config: _,
            since,
            recency_weight,
            recency_half_life_days,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            if use_daemon && let Ok(rt) = tokio::runtime::Runtime::new() {
                let connect = rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None));
                if let Ok(mut client) = connect {
                    let req = nestweaver_proto::ProjectContextRequest {
                        project: name.clone(),
                        token_budget: token_budget as i32,
                        kinds: vec![],
                        include_components,
                        intent: String::new(),
                        include_seeds: false,
                        since: since.clone().unwrap_or_default(),
                        recency_weight,
                        recency_half_life_days,
                    };
                    let rpc = rt.block_on(async {
                        client
                            .inner_mut()
                            .get_project_context(req)
                            .await
                            .map(|r| r.into_inner())
                    });
                    match rpc {
                        Ok(resp) => {
                            let value: serde_json::Value = serde_json::from_str(&resp.result_json)?;
                            render_project_context_daemon_response(&value, json, token_budget);
                            return Ok((EXIT_SUCCESS, None));
                        }
                        Err(status) => {
                            tracing::info!(
                                "daemon GetProjectContext failed ({}); falling back to direct mode",
                                status.message()
                            );
                        }
                    }
                }
            }

            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            if tantivy.is_none() {
                tracing::info!("Tantivy index unavailable — BM25 search disabled for this query");
            }

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

            // Collect member UIDs for the post-PPR boost. These are the
            // notes and symbols declared as belonging to this project.
            // Member note UIDs are tracked separately: they get seeded into
            // PPR and surfaced into `connected` (Bug #12).
            let mut member_uids: Vec<String> = Vec::new();
            let mut member_note_uids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let note_uids = store
                .list_project_note_uids(&project.uid)
                .map_err(|e| anyhow::anyhow!(e))?;
            member_note_uids.extend(note_uids.iter().cloned());
            member_uids.extend(note_uids);
            let sym_uids = store
                .list_project_symbol_uids(&project.uid)
                .map_err(|e| anyhow::anyhow!(e))?;
            member_uids.extend(sym_uids);

            let comp_uids = if include_components {
                store
                    .list_project_component_uids(&project.uid)
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                vec![]
            };
            for comp_uid in &comp_uids {
                let comp_notes = store.list_project_note_uids(comp_uid).unwrap_or_default();
                member_note_uids.extend(comp_notes.iter().cloned());
                member_uids.extend(comp_notes);
                member_uids.extend(store.list_project_symbol_uids(comp_uid).unwrap_or_default());
            }

            // Deduplicate members.
            let mut seen = std::collections::HashSet::new();
            member_uids.retain(|u| seen.insert(u.clone()));

            if member_uids.is_empty() {
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

            // Seed PPR from the project node, its components, and the
            // project's member notes (Bug #12). Seeding the notes guarantees
            // they survive the `min_score` filter in PPR — when a project
            // declares repos, the project node's mass is split across tens of
            // thousands of PROJECT_INCLUDES_SYMBOL edges, leaving each note
            // below threshold so it never reaches `connected`.
            //
            // Member symbols suffer the identical fan-out, so seed the
            // top-K of them by PageRank as well. Without this, a project
            // that declares any repo returns notes-only context even after
            // `materialize-projects` writes hundreds of thousands of
            // PROJECT_INCLUDES_SYMBOL edges (Bug #18 / wave-5 regression).
            const PROJECT_SYMBOL_SEED_LIMIT: usize = 100;
            let mut member_symbol_uids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let top_symbols = store
                .list_project_symbol_uids_by_pagerank(&project.uid, PROJECT_SYMBOL_SEED_LIMIT)
                .map_err(|e| anyhow::anyhow!(e))?;
            member_symbol_uids.extend(top_symbols.iter().cloned());
            for comp_uid in &comp_uids {
                let comp_top = store
                    .list_project_symbol_uids_by_pagerank(comp_uid, PROJECT_SYMBOL_SEED_LIMIT)
                    .unwrap_or_default();
                member_symbol_uids.extend(comp_top);
            }

            let mut ppr_seeds: Vec<String> = vec![project.uid.clone()];
            ppr_seeds.extend(comp_uids);
            ppr_seeds.extend(member_note_uids.iter().cloned());
            ppr_seeds.extend(member_symbol_uids.iter().cloned());

            let defaults = HybridSearchConfig::default();
            let aliases = load_alias_sidecar(&db_path);
            match build_brain_context_hybrid_with_aliases(
                &store,
                &ppr_seeds,
                tantivy.as_ref(),
                &defaults,
                &aliases,
                Some(&db_path),
                Some(nestweaver_store::QueryIntent::ProjectContext),
            ) {
                Ok(mut result) => {
                    // Surface the project's curated member notes into
                    // `connected` (Bug #12). Seeded notes land in `seeds`,
                    // which print_brain_context_json does not render.
                    nestweaver_engine::promote_member_notes_into_connected(
                        &mut result,
                        &member_note_uids,
                    );
                    // Surface the seeded member symbols into `connected`
                    // for the same reason (companion to the notes promotion).
                    nestweaver_engine::promote_member_symbols_into_connected(
                        &mut result,
                        &member_symbol_uids,
                    );
                    // Drop Heading/Section duplicates: notes-heavy projects
                    // would otherwise spend ~25% of a 2000-token budget on
                    // pairs that share `(file, title)` and add no information.
                    nestweaver_engine::dedup_heading_section_pairs(&mut result);

                    // Post-PPR scope boost: multiply relevance for nodes that
                    // belong to the project so declared content ranks highest.
                    let member_set: std::collections::HashSet<&str> =
                        member_uids.iter().map(|s| s.as_str()).collect();
                    for node in &mut result.connected {
                        if member_set.contains(node.uid.as_str()) {
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

                    // Compute seed token cost and allocate the remainder to
                    // connected. Don't double-count items the promotion helpers
                    // copied from `seeds` into `connected` — those tokens belong
                    // to the connected budget, not the seed overhead.
                    let connected_uids: std::collections::HashSet<&str> =
                        result.connected.iter().map(|n| n.uid.as_str()).collect();
                    let seed_tokens: usize = result
                        .seeds
                        .iter()
                        .filter(|n| !connected_uids.contains(n.uid.as_str()))
                        .map(render_cost_tokens)
                        .sum();
                    let remaining_budget = token_budget.saturating_sub(seed_tokens);
                    let cut = token_budgeted_truncate(&result.connected, remaining_budget);
                    let connected_tokens: usize = result
                        .connected
                        .iter()
                        .take(cut)
                        .map(render_cost_tokens)
                        .sum();
                    let used_tokens = seed_tokens + connected_tokens;
                    // Load external_refs from the extension sidecar so the
                    // local (--no-daemon) path matches the daemon/MCP wrapper
                    // shape — agents rely on this for Workfront / wiki PRD
                    // surfacing.
                    let ext_store = nestweaver_engine::load_extensions(&db_path);
                    let external_refs =
                        nestweaver_engine::get_all_properties(&ext_store, &project.uid)
                            .get("external_refs")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                    if json {
                        print_project_context_json(
                            &project,
                            &result,
                            cut,
                            used_tokens,
                            token_budget,
                            &external_refs,
                        )?;
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

        Commands::Investigate {
            query,
            scope,
            token_budget,
            root,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({
                    "query": query,
                    "token_budget": token_budget,
                });
                if let Some(ref s) = scope {
                    args["scope"] = serde_json::json!(s);
                }
                if let Some(ref r) = root {
                    args["root"] = serde_json::json!(r);
                }
                if let Some(value) = try_daemon_json_rpc(true, &db_path, None, "investigate", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            let root = root.unwrap_or_else(detect_repo_root);
            let scope = scope.unwrap_or_else(|| "vault".to_string());
            let result = nestweaver_engine::investigate(
                &store,
                tantivy.as_ref(),
                Some(&db_path),
                &root,
                &query,
                &scope,
                Some(token_budget),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Bundle: {}  (query: {:?})", result.bundle_id, result.query);
                println!(
                    "{} domain(s), {} entr{}{}",
                    result.domains.len(),
                    result.entries.len(),
                    if result.entries.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    if result.more_available > 0 {
                        format!(
                            " ({} more available — raise --token-budget)",
                            result.more_available
                        )
                    } else {
                        String::new()
                    }
                );
                for d in &result.domains {
                    println!("\n[{}]", d.label);
                    for asset_id in &d.members {
                        if let Some(e) = result.entries.iter().find(|e| &e.asset_id == asset_id) {
                            let marker = if e.asset_id == d.entry_point {
                                "*"
                            } else {
                                " "
                            };
                            println!(
                                "  {marker} {}  {} ({})  {}",
                                e.asset_id, e.title, e.kind, e.location
                            );
                            if let Some(s) = &e.summary {
                                let truncated = if e.inline_body.is_some() && !e.body_complete {
                                    " [truncated]"
                                } else {
                                    ""
                                };
                                println!("      {s}{truncated}");
                            }
                        }
                    }
                }
                println!(
                    "\nDrill in: nestweaver investigate-expand {} --targets <asset_id,...>",
                    result.bundle_id
                );
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::InvestigateExpand {
            bundle_id,
            targets,
            root,
            json,
            db,
        } => {
            if targets.is_empty() {
                eprintln!("Error: --targets must list at least one asset_id or uid");
                return Ok((EXIT_ERROR, None));
            }
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({
                    "bundle_id": bundle_id,
                    "targets": targets,
                });
                if let Some(ref r) = root {
                    args["root"] = serde_json::json!(r);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "investigate_expand", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let root = root.unwrap_or_else(detect_repo_root);
            let result = nestweaver_engine::investigate_expand(
                &store, &db_path, &root, &bundle_id, &targets,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                if !result.unresolved.is_empty() {
                    println!("Unresolved targets: {}", result.unresolved.join(", "));
                }
                for e in &result.expanded {
                    println!("\n=== {}  {} ({}) ===", e.asset_id, e.title, e.location);
                    if let Some(body) = &e.inline_body {
                        println!("{body}");
                    }
                    let neighbors: Vec<&nestweaver_engine::NeighborRef> = result
                        .neighbors
                        .iter()
                        .filter(|n| n.of == e.asset_id)
                        .collect();
                    if !neighbors.is_empty() {
                        println!("-- neighbors --");
                        for n in neighbors {
                            println!("  [{}] {} ({})", n.relation, n.title, n.uid);
                        }
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::InvestigateHydrate {
            bundle_id,
            token_budget,
            root,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({
                    "bundle_id": bundle_id,
                    "token_budget": token_budget,
                });
                if let Some(ref r) = root {
                    args["root"] = serde_json::json!(r);
                }
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "investigate_hydrate", args)
                {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let root = root.unwrap_or_else(detect_repo_root);
            let result = nestweaver_engine::investigate_hydrate(
                &store,
                &db_path,
                &root,
                &bundle_id,
                Some(token_budget),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let truncated_count = result
                    .entries
                    .iter()
                    .filter(|e| e.inline_body.is_some() && !e.body_complete)
                    .count();
                println!(
                    "Hydrated {} entr{} in bundle {}{}",
                    result.hydrated,
                    if result.hydrated == 1 { "y" } else { "ies" },
                    result.bundle_id,
                    if truncated_count > 0 {
                        format!(" ({truncated_count} truncated — use read_symbols for full source)")
                    } else {
                        String::new()
                    }
                );
            }
            Ok((EXIT_SUCCESS, None))
        }

        Commands::MaterializeProjects { config, db } => {
            let config_path = &config;
            let instance_config = nestweaver_engine::InstanceConfig::from_file(config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let instance_id = &instance_config.instance_id;
            let db_path = db.unwrap_or_else(default_db_path);

            if cli.no_daemon {
                eprintln!(
                    "Warning: --no-daemon is ignored for write operations; routing through daemon."
                );
            }

            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    Some(config_path),
                ))
                .context("failed to connect to daemon")?;

            let mut stream = rt
                .block_on(client.materialize_projects(&config_path.to_string_lossy(), instance_id))
                .context("materialize_projects RPC failed")?;

            let mut had_error = false;
            rt.block_on(async {
                while let Some(progress) = stream.message().await? {
                    eprintln!("{}", progress.message);
                    if progress.phase == nestweaver_proto::Phase::Error as i32 {
                        had_error = true;
                    }
                }
                Ok::<_, anyhow::Error>(())
            })?;

            if had_error {
                return Ok((EXIT_ERROR, None));
            }

            Ok((EXIT_SUCCESS, None))
        }

        Commands::DetectImplicitProjects { vault, db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            if !vault.exists() || !vault.is_dir() {
                eprintln!("Error: vault path is not a directory: {}", vault.display());
                return Ok((EXIT_ERROR, None));
            }

            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let args = serde_json::json!({
                    "vault": vault.to_string_lossy(),
                });
                if let Some(value) =
                    try_daemon_json_rpc(true, &db_path, None, "detect_implicit_projects", args)
                {
                    let detected: Vec<String> = serde_json::from_value(value).unwrap_or_default();
                    if detected.is_empty() {
                        println!("No implicit projects detected in {}", vault.display());
                    } else {
                        println!("Detected {} implicit project(s):", detected.len());
                        for slug in &detected {
                            println!("  {slug}");
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;

            // Resolve vault UID the same way the indexer does.
            let canonical = std::fs::canonicalize(&vault).unwrap_or_else(|_| vault.clone());
            let instance_id = "default";
            let vault_uid = nestweaver_schema::vault_uid(instance_id, &canonical.to_string_lossy());

            let detected = detect_implicit_projects(&store, &vault, &vault_uid, instance_id)?;

            if detected.is_empty() {
                println!("No implicit projects detected in {}", vault.display());
            } else {
                println!("Detected {} implicit project(s):", detected.len());
                for slug in &detected {
                    println!("  {slug}");
                }
            }

            Ok((EXIT_SUCCESS, None))
        }

        Commands::Index {
            repo,
            instance,
            db,
            force,
            name,
            with_trigrams,
            with_git_activity,
            config,
        } => {
            let repo_path = match repo {
                Some(p) => p,
                None => detect_repo_root(),
            };
            let db_path = resolve_index_db_path(db, &repo_path);

            if use_daemon {
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ))?;

                let req = nestweaver_proto::IndexRepoRequest {
                    repo_path: repo_path.display().to_string(),
                    name: name.unwrap_or_default(),
                    force,
                    with_trigrams,
                    with_git_activity,
                };

                rt.block_on(async {
                    let mut stream = client.inner_mut().index_repo(req).await?.into_inner();
                    while let Some(progress) = stream.message().await? {
                        let phase_name = match progress.phase {
                            0 => "Discovering",
                            1 => "Parsing",
                            2 => "Resolving",
                            3 => "Writing",
                            4 => "PageRank",
                            5 => "Done",
                            6 => "Error",
                            _ => "Unknown",
                        };
                        eprintln!("[{phase_name}] {}", progress.message);
                    }
                    Ok::<_, anyhow::Error>(())
                })?;

                return Ok((EXIT_SUCCESS, None));
            }

            let instance_id = instance.as_deref().unwrap_or("default");

            let repo_url = format!("file://{}", repo_path.display());

            // Direct-write fallback for test/CI (NESTWEAVER_NO_DAEMON=1).
            let daemon_was_running = stop_daemon_if_running(&db_path);

            out.status(&format!("Indexing {}", repo_path.display()));

            let (files_count, symbols_count, edges_count);

            if force {
                // Full re-index requested explicitly.
                let result = index_directory_with_options(
                    &repo_path,
                    &db_path,
                    instance_id,
                    &repo_url,
                    "local",
                    true,
                    name.as_deref(),
                )
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
                let inc = incremental_index_with_name(
                    &repo_path,
                    &db_path,
                    instance_id,
                    &repo_url,
                    name.as_deref(),
                )
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

            // PageRank is deferred to first query (lazy evaluation in
            // GraphStore::ensure_pagerank_loaded) so the index path stays fast.
            out.status("PageRank will be computed on first query.");

            // Feature F12: mine git history and write the recency sidecar so
            // subsequent commands demote dormant code at rank-read time.
            // Honor the per-repo `use_git_activity = false` opt-out when a
            // config matches this repo's URL.
            let repo_opted_out = load_instance_config_opt(config.as_deref())
                .map(|cfg| {
                    cfg.repos
                        .iter()
                        .find(|r| r.url == repo_url)
                        .and_then(|r| r.use_git_activity)
                        == Some(false)
                })
                .unwrap_or(false);

            if with_git_activity && repo_opted_out {
                out.status(
                    "Repo has use_git_activity = false in config; skipping git-activity sidecar.",
                );
            } else if with_git_activity {
                out.status("Mining git activity...");
                let scores = nestweaver_engine::git_activity::compute_git_activity(&repo_path);
                if scores.is_empty() {
                    out.status("No usable git history found; git-activity sidecar not written.");
                } else {
                    let ga_path = nestweaver_engine::sidecar_path(&db_path, ".gitactivity.json");
                    nestweaver_engine::git_activity::save_git_activity(&scores, &ga_path)
                        .with_context(|| "save git activity sidecar")?;
                    out.status(&format!(
                        "Git activity sidecar written ({} files scored).",
                        scores.len()
                    ));
                }
            }

            // Co-change mining (piggybacks on --with-git-activity)
            if with_git_activity && !repo_opted_out {
                out.status("Mining co-changes...");
                match compute_cochanges(&repo_path, 500, 3, 0.30) {
                    Ok(edges) => {
                        let cochange_path =
                            nestweaver_engine::sidecar_path(&db_path, ".cochange.json");
                        if let Err(e) = save_cochange_sidecar(&edges, &cochange_path) {
                            tracing::warn!("failed to save co-change sidecar: {e}");
                        }
                        out.status(&format!("Found {} co-change pairs.", edges.len()));
                    }
                    Err(e) => {
                        tracing::warn!("co-change mining failed: {e}");
                    }
                }
            }

            if with_trigrams {
                out.status("Building trigram index...");
                let store = GraphStore::open(&db_path)
                    .with_context(|| format!("failed to open database at {}", db_path.display()))?;
                let postings = store
                    .build_trigram_index()
                    .with_context(|| "build_trigram_index")?;
                out.status(&format!("Trigram index built ({postings} postings)."));
            }

            let stats = format!(
                "{} files, {} symbols, {} edges in {}",
                files_count,
                symbols_count,
                edges_count,
                format_elapsed(t0.elapsed())
            );

            // Restart daemon if we stopped it for direct-mode indexing
            if daemon_was_running {
                restart_daemon(&db_path, config.as_deref().map(std::path::Path::new));
            }

            Ok((EXIT_SUCCESS, Some(stats)))
        }

        Commands::Daemon { action, db } => {
            let db_path = db
                .or_else(|| {
                    std::env::var("NESTWEAVER_DB")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map(PathBuf::from)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("No database path provided. Use --db or set NESTWEAVER_DB.")
                })?;
            // Don't pre-canonicalize — instance_id_from_db_path handles it
            // internally with consistent fallback for non-existent files.
            let instance_id = nestweaver_daemon::instance_id_from_db_path(&db_path);
            let runtime_dir = nestweaver_daemon::runtime_dir(&instance_id);
            let log_dir = nestweaver_daemon::log_dir(&instance_id);
            let pidfile = nestweaver_daemon::pidfile_path(&instance_id);
            let socket = nestweaver_daemon::socket_path(&instance_id);
            let log_file = nestweaver_daemon::log_path(&instance_id);

            match action {
                DaemonAction::Start {
                    idle_timeout,
                    config,
                    track_interactions,
                } => {
                    if track_interactions {
                        eprintln!(
                            "note: --track-interactions is an MCP flag, not a daemon flag. \
                             Use: nestweaver mcp --track-interactions"
                        );
                    }
                    std::fs::create_dir_all(&runtime_dir).with_context(|| {
                        format!("create runtime dir: {}", runtime_dir.display())
                    })?;
                    std::fs::create_dir_all(&log_dir)
                        .with_context(|| format!("create log dir: {}", log_dir.display()))?;

                    // Atomically detect another running or starting daemon via
                    // a non-blocking exclusive flock on the pidfile. daemonize2
                    // holds LOCK_EX on the pidfile for the daemon's entire
                    // lifetime (see autostart.rs for the matching consumer),
                    // so a successful flock here proves no daemon owns it.
                    //
                    // This replaces a previous `kill(pid, 0)` check that had a
                    // TOCTOU window: two concurrent `daemon start` invocations
                    // (e.g. from `launchctl kickstart -k` or rapid respawn)
                    // could both pass the kill check and race for the DB write
                    // lock, with the loser dying with "Could not set lock on
                    // file ... another process may hold the write lock".
                    use std::os::unix::io::AsRawFd;
                    let pid_lock = std::fs::OpenOptions::new()
                        .create(true)
                        .read(true)
                        .write(true)
                        .truncate(false)
                        .open(&pidfile)
                        .with_context(|| format!("open pidfile: {}", pidfile.display()))?;
                    let pid_lock_fd = pid_lock.as_raw_fd();
                    let lock_ret =
                        unsafe { libc::flock(pid_lock_fd, libc::LOCK_EX | libc::LOCK_NB) };
                    if lock_ret != 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::WouldBlock {
                            let pid_text = std::fs::read_to_string(&pidfile).unwrap_or_default();
                            let pid_trimmed = pid_text.trim();
                            if pid_trimmed.is_empty() {
                                eprintln!("Daemon already running (starting up).");
                            } else {
                                eprintln!("Daemon already running (PID {pid_trimmed}).");
                            }
                            return Ok((EXIT_SUCCESS, None));
                        }
                        anyhow::bail!("flock on pidfile failed: {err}");
                    }

                    // We hold the lock — no other daemon is running or
                    // starting. Any PID in the file is stale; daemonize2 will
                    // overwrite it after the double-fork.
                    // Release the flock immediately before daemonize2's
                    // start() so it can acquire its own lock on the same
                    // pidfile; the race window between release and reacquire
                    // is microseconds, far smaller than the prior TOCTOU
                    // window of the kill(pid, 0) check.

                    let stdout_file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_file)
                        .with_context(|| format!("open log file: {}", log_file.display()))?;
                    let stderr_file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_file)
                        .with_context(|| {
                            format!("open log file for stderr: {}", log_file.display())
                        })?;

                    eprintln!(
                        "Starting daemon for {} (instance {instance_id})...",
                        db_path.display()
                    );
                    eprintln!("  PID file: {}", pidfile.display());
                    eprintln!("  Socket:   {}", socket.display());
                    eprintln!("  Log:      {}", log_file.display());

                    let daemonize = daemonize2::Daemonize::new()
                        .pid_file(&pidfile)
                        .stdout(stdout_file)
                        .stderr(stderr_file)
                        .working_directory(".");

                    unsafe { libc::flock(pid_lock_fd, libc::LOCK_UN) };
                    drop(pid_lock);

                    match unsafe { daemonize.start() } {
                        Ok(()) => {
                            // We are now the daemon process.
                            let idle = if idle_timeout > 0 {
                                Some(std::time::Duration::from_secs(idle_timeout))
                            } else {
                                None
                            };
                            let rt = tokio::runtime::Runtime::new()
                                .expect("failed to create tokio runtime");
                            let config_path = config.clone();
                            rt.block_on(async {
                                if let Err(e) = nestweaver_daemon::run_server(
                                    &db_path,
                                    idle,
                                    config_path.as_deref(),
                                )
                                .await
                                {
                                    eprintln!("Daemon error: {e:#}");
                                    std::process::exit(1);
                                }
                            });
                        }
                        Err(e) => {
                            anyhow::bail!("Failed to daemonize: {e}");
                        }
                    }
                    Ok((EXIT_SUCCESS, None))
                }

                DaemonAction::Stop => {
                    let pid_str = std::fs::read_to_string(&pidfile)
                        .with_context(|| format!("read pidfile: {}", pidfile.display()))?;
                    let pid: i32 = pid_str
                        .trim()
                        .parse()
                        .with_context(|| "parse PID from pidfile")?;

                    eprintln!("Stopping daemon (PID {pid})...");
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }

                    // Poll for up to 5 seconds.
                    for _ in 0..50 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        if unsafe { libc::kill(pid, 0) } != 0 {
                            eprintln!("Daemon stopped.");
                            let _ = std::fs::remove_file(&pidfile);
                            let _ = std::fs::remove_file(&socket);
                            return Ok((EXIT_SUCCESS, None));
                        }
                    }

                    // Force kill.
                    eprintln!("Daemon did not exit; sending SIGKILL...");
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let _ = std::fs::remove_file(&pidfile);
                    let _ = std::fs::remove_file(&socket);
                    eprintln!("Daemon killed.");
                    Ok((EXIT_SUCCESS, None))
                }

                DaemonAction::Status => {
                    if let Ok(pid_str) = std::fs::read_to_string(&pidfile)
                        && let Ok(pid) = pid_str.trim().parse::<i32>()
                        && unsafe { libc::kill(pid, 0) } == 0
                    {
                        println!("Daemon is running (PID {pid})");
                        println!("  DB:     {}", db_path.display());
                        println!("  Socket: {}", socket.display());
                        println!("  Log:    {}", log_file.display());
                        return Ok((EXIT_SUCCESS, None));
                    }
                    println!("Daemon is not running.");
                    Ok((EXIT_SUCCESS, None))
                }

                DaemonAction::Restart {
                    idle_timeout,
                    config,
                } => {
                    // Stop if running.
                    if let Ok(pid_str) = std::fs::read_to_string(&pidfile)
                        && let Ok(pid) = pid_str.trim().parse::<i32>()
                        && unsafe { libc::kill(pid, 0) } == 0
                    {
                        eprintln!("Stopping daemon (PID {pid})...");
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                        for _ in 0..50 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            if unsafe { libc::kill(pid, 0) } != 0 {
                                break;
                            }
                        }
                        if unsafe { libc::kill(pid, 0) } == 0 {
                            unsafe { libc::kill(pid, libc::SIGKILL) };
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        let _ = std::fs::remove_file(&pidfile);
                        let _ = std::fs::remove_file(&socket);
                        eprintln!("Daemon stopped.");
                    }

                    // Re-exec ourselves to start the daemon fresh.
                    let exe =
                        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nestweaver"));
                    let mut start_args: Vec<String> = vec![
                        "daemon".to_string(),
                        "--db".to_string(),
                        db_path.display().to_string(),
                        "start".to_string(),
                        "--idle-timeout".to_string(),
                        idle_timeout.to_string(),
                    ];
                    if let Some(cfg) = config.as_deref() {
                        start_args.push("--config".to_string());
                        start_args.push(cfg.display().to_string());
                    }
                    let status = std::process::Command::new(&exe)
                        .args(&start_args)
                        .status()
                        .with_context(|| "failed to restart daemon")?;
                    if !status.success() {
                        anyhow::bail!("daemon start failed with {status}");
                    }
                    Ok((EXIT_SUCCESS, None))
                }
            }
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

/// Like [`resolve_uid`] but applies an optional repo filter to narrow ambiguous
/// matches. When `repo_filter` is `Some`, only symbols belonging to a repo
/// whose display name matches the filter are kept. Also matches against
/// file_path prefix and UID substring as fallbacks.
fn resolve_uid_with_repo_filter(
    store: &GraphStore,
    name_or_uid: &str,
    repo_filter: Option<&str>,
) -> anyhow::Result<ResolveResult> {
    let result = resolve_uid(store, name_or_uid)?;
    match (&result, repo_filter) {
        (ResolveResult::Ambiguous(candidates), Some(filter)) => {
            let filter_lower = filter.to_lowercase();

            // Build a repo_uid → display_name map for matching
            let repos = list_repos(store, None)?;
            let repo_names: std::collections::HashMap<String, String> = repos
                .iter()
                .map(|r| (r.uid.clone(), nestweaver_engine::repo_display_name(r)))
                .collect();

            let filtered: Vec<Symbol> = candidates
                .iter()
                .filter(|s| {
                    // Match by repo display name (primary — supports --name overrides)
                    if let Some(name) = repo_names.get(&s.repo_uid)
                        && name.to_lowercase().contains(&filter_lower)
                    {
                        return true;
                    }
                    // Fallback: file_path prefix or UID substring
                    s.file_path.to_lowercase().starts_with(&filter_lower)
                        || s.uid.to_lowercase().contains(&filter_lower)
                })
                .cloned()
                .collect();
            match filtered.len() {
                0 => Ok(ResolveResult::NotFound),
                1 => Ok(ResolveResult::Found(filtered[0].uid.clone())),
                _ => Ok(ResolveResult::Ambiguous(filtered)),
            }
        }
        _ => Ok(result),
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

/// Resolve a node UID to its file-path location for ranking-prior matching.
/// Mirrors the location each kind renders with in brain results.
fn ranking_location_for_uid(store: &nestweaver_store::GraphStore, uid: &str) -> Option<String> {
    if uid.starts_with("sym:") {
        let s = store.lookup_symbol(uid).ok()?;
        Some(format!("{}:{}", s.file_path, s.start_line))
    } else if uid.starts_with("note:") {
        store.lookup_note(uid).ok().map(|n| n.file_path)
    } else if uid.starts_with("sec:") {
        let sec = store.lookup_section(uid).ok()?;
        store.lookup_note(&sec.note_uid).ok().map(|n| n.file_path)
    } else if uid.starts_with("head:") {
        let h = store.lookup_heading(uid).ok()?;
        store.lookup_note(&h.note_uid).ok().map(|n| n.file_path)
    } else {
        None
    }
}

/// Build a [`HybridSearchConfig`] for the eval harness with the given PRF
/// toggle, otherwise defaults (identical to the product's default retrieval).
fn eval_hybrid_config(prf: bool) -> HybridSearchConfig {
    HybridSearchConfig {
        prf,
        ..HybridSearchConfig::default()
    }
}

/// Print the per-query table and aggregate for an `EvalReport` (human form).
fn print_eval_report(report: &nestweaver_engine::EvalReport) {
    println!("Query                                              nDCG@10    MRR  P@5");
    println!("{}", "-".repeat(78));
    for row in &report.per_query {
        let q: String = row.query.chars().take(48).collect();
        println!(
            "{q:<48}  {:>7.4}  {:>5.3}  {:>4.2}",
            row.ndcg10, row.mrr, row.p_at_5
        );
    }
    println!("{}", "-".repeat(78));
    println!(
        "MEAN over {} quer{}:  nDCG@10={:.4}  MRR={:.4}  P@5={:.4}",
        report.n,
        if report.n == 1 { "y" } else { "ies" },
        report.mean_ndcg10,
        report.mean_mrr,
        report.mean_p5,
    );
}

/// Dispatch an `eval` subcommand (P0.3 retrieval-quality harness).
fn run_eval_cmd(command: EvalCommands) -> anyhow::Result<i32> {
    // Honest-framing banner shown on every human-readable run.
    const HONEST_NOTE: &str = "Note: meaningful evaluation requires REAL human relevance labels over your actual\n      corpus. A tiny/synthetic set is NOT authoritative — inspect per-query\n      win/loss and confidence, and use time/query-based splits, before trusting a\n      small mean delta.";

    match command {
        EvalCommands::Run {
            queries,
            db,
            json,
            prf,
            rerank,
        } => {
            let queries_data = nestweaver_engine::load_judged_queries(&queries)?;
            let db_path = resolve_db_with_config(db, None)?;
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            let aliases = load_alias_sidecar(&db_path);

            let cfg = eval_hybrid_config(prf);
            let report = nestweaver_engine::run_eval(
                &store,
                tantivy.as_ref(),
                &queries_data,
                &cfg,
                &aliases,
                Some(&db_path),
                rerank,
            )?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_eval_report(&report);
                println!("\n{HONEST_NOTE}");
            }
            Ok(EXIT_SUCCESS)
        }
        EvalCommands::Compare {
            queries,
            db,
            json,
            prf,
            rerank,
        } => {
            if !prf && !rerank {
                anyhow::bail!("eval compare needs a feature to toggle: pass --prf and/or --rerank");
            }
            let queries_data = nestweaver_engine::load_judged_queries(&queries)?;
            let db_path = resolve_db_with_config(db, None)?;
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            let aliases = load_alias_sidecar(&db_path);

            // Baseline: feature(s) OFF. Treatment: the chosen toggle(s) ON.
            // PRF lives in the HybridSearchConfig; rerank is a run_eval flag.
            let baseline_cfg = eval_hybrid_config(false);
            let treatment_cfg = eval_hybrid_config(prf);

            let run = |cfg: &HybridSearchConfig, do_rerank: bool| {
                nestweaver_engine::run_eval(
                    &store,
                    tantivy.as_ref(),
                    &queries_data,
                    cfg,
                    &aliases,
                    Some(&db_path),
                    do_rerank,
                )
            };
            let baseline = run(&baseline_cfg, false)?;
            let treatment = run(&treatment_cfg, rerank)?;

            let mut toggles = Vec::new();
            if prf {
                toggles.push("prf");
            }
            if rerank {
                toggles.push("rerank");
            }
            let label = toggles.join("+");
            let cmp = nestweaver_engine::compare_reports(
                format!("{label}-off"),
                baseline,
                format!("{label}-on"),
                treatment,
            );

            if json {
                println!("{}", serde_json::to_string_pretty(&cmp)?);
            } else {
                println!(
                    "Comparison: {} (baseline) vs {} (treatment) over {} quer{}",
                    cmp.baseline_label,
                    cmp.treatment_label,
                    cmp.baseline.n,
                    if cmp.baseline.n == 1 { "y" } else { "ies" },
                );
                println!("  baseline  mean nDCG@10 = {:.4}", cmp.baseline.mean_ndcg10);
                println!(
                    "  treatment mean nDCG@10 = {:.4}",
                    cmp.treatment.mean_ndcg10
                );
                println!(
                    "  delta = {:+.4}  ({:+.1}% relative)",
                    cmp.mean_ndcg10_delta,
                    cmp.mean_ndcg10_rel_delta * 100.0,
                );
                println!(
                    "  per-query: {} win(s), {} loss(es), {} tie(s)",
                    cmp.wins, cmp.losses, cmp.ties,
                );
                let gate = cmp.mean_ndcg10_rel_delta >= 0.05;
                println!(
                    "  >= 5% nDCG@10 gate: {}",
                    if gate {
                        "MET (mean only — confirm with per-query win/loss + a larger set)"
                    } else {
                        "NOT met"
                    }
                );
                println!("\n{HONEST_NOTE}");
            }
            Ok(EXIT_SUCCESS)
        }
    }
}

/// Dispatch a `ranking` subcommand.
fn run_ranking(
    command: RankingCommands,
    t0: std::time::Instant,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        RankingCommands::Explain {
            uid,
            json,
            db,
            config,
            base_relevance,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;

            let ranking = load_instance_config_opt(config.as_deref())
                .map(|c| c.ranking)
                .unwrap_or_default();

            // Resolve the node's location (the path globs are matched against).
            // Exit 2 when the uid doesn't resolve to a node, consistent with
            // `symbol`/`impact`/`ranking rank`.
            let location = match ranking_location_for_uid(&store, &uid) {
                Some(loc) => loc,
                None => {
                    if json {
                        println!("{}", serde_json::json!({"error": "not found", "uid": uid}));
                    } else {
                        eprintln!("uid '{uid}' not found.");
                    }
                    return Ok((EXIT_NOT_FOUND, None));
                }
            };

            // Delegate the matching + clamping to the engine so the math matches
            // exactly what brain context / search apply.
            let (matched, final_relevance) =
                nestweaver_engine::explain_ranking_prior(&location, base_relevance, &ranking);

            if json {
                let matched_json = match &matched {
                    Some((glob, mult)) => serde_json::json!({
                        "glob": glob,
                        "multiplier": mult,
                    }),
                    None => serde_json::Value::Null,
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uid": uid,
                        "location": location,
                        "base_relevance": base_relevance,
                        "matched_rule": matched_json,
                        "final_relevance": final_relevance,
                    }))?
                );
            } else {
                println!("uid:             {uid}");
                println!("location:        {location}");
                println!("base_relevance:  {base_relevance}");
                match &matched {
                    Some((glob, mult)) => {
                        println!("matched_rule:    {glob} (x{mult})");
                    }
                    None => println!("matched_rule:    none"),
                }
                println!("final_relevance: {final_relevance}");
            }
            Ok((
                EXIT_SUCCESS,
                Some(format!("done in {}", format_elapsed(t0.elapsed()))),
            ))
        }
        RankingCommands::Rank {
            uid,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;

            // Apply the configured git-activity weight if a config carries one.
            if let Some(cfg) = load_instance_config_opt(config.as_deref()) {
                store.set_git_activity_weight(cfg.ranking.git_activity_weight);
            }

            // Resolve name-or-uid → uid, then load the symbol.
            let resolved = match resolve_uid(&store, &uid)? {
                ResolveResult::Found(u) => u,
                ResolveResult::NotFound => {
                    eprintln!("Symbol not found: {uid}");
                    return Ok((EXIT_NOT_FOUND, None));
                }
                ResolveResult::Ambiguous(matches) => {
                    eprintln!("Ambiguous symbol '{uid}' — {} matches:", matches.len());
                    for m in matches.iter().take(10) {
                        eprintln!("  {} ({}:{})", m.uid, m.file_path, m.start_line);
                    }
                    return Ok((EXIT_AMBIGUOUS, None));
                }
            };

            let sym = store
                .lookup_symbol(&resolved)
                .map_err(|e| anyhow::anyhow!(e))?;
            let base_pagerank = store
                .pagerank_scores()
                .get(&resolved)
                .copied()
                .unwrap_or(0.0);
            let git_activity_score = store.git_activity_score(&sym.file_path);
            let weight = store.git_activity_weight();
            let multiplier = nestweaver_store::git_activity_multiplier(git_activity_score, weight);
            let final_rank = base_pagerank * multiplier;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uid": resolved,
                        "file_path": sym.file_path,
                        "base_pagerank": base_pagerank,
                        "git_activity_score": git_activity_score,
                        "git_activity_weight": weight,
                        "multiplier": multiplier,
                        "final_rank": final_rank,
                    }))?
                );
            } else {
                println!("uid:                {resolved}");
                println!("file_path:          {}", sym.file_path);
                println!("base_pagerank:      {base_pagerank:.8}");
                match git_activity_score {
                    Some(s) => println!("git_activity_score: {s:.4}"),
                    None => println!("git_activity_score: (none → neutral)"),
                }
                println!("multiplier:         {multiplier:.4} (weight {weight})");
                println!("final_rank:         {final_rank:.8}");
            }
            Ok((
                EXIT_SUCCESS,
                Some(format!("done in {}", format_elapsed(t0.elapsed()))),
            ))
        }
    }
}

/// Dispatch a `brain` subcommand.
/// Current wall-clock time as Unix epoch seconds (f64).
fn now_epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn run_memory(
    command: MemoryCommands,
    t0: std::time::Instant,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        MemoryCommands::Lint { json, db, config } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;
            let report = nestweaver_engine::memory_lint(&store, now_epoch_secs())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Memory lint:");
                println!("  stale notes:           {}", report.stale.len());
                println!("  contradictions:        {}", report.contradictions.len());
                println!("  orphans:               {}", report.orphans.len());
                println!("  broken wikilinks:      {}", report.broken_wikilinks.len());
                println!(
                    "  supersession chains:   {}",
                    report.supersession_chains.len()
                );
                println!("  schema drift:          {}", report.schema_drift.len());
                println!(
                    "  dangling relationships: {}",
                    report.dangling_relationships.len()
                );
                for s in &report.stale {
                    println!("  stale: {} ({} days)", s.file_path, s.days_stale);
                }
                for c in &report.contradictions {
                    println!("  contradiction cycle: {}", c.cycle.join(" → "));
                }
                for d in &report.dangling_relationships {
                    println!(
                        "  dangling: {} -[{}]-> {} (missing)",
                        d.source_uid, d.edge_type, d.target_uid
                    );
                }
            }
            let issues = report.stale.len()
                + report.contradictions.len()
                + report.supersession_chains.len()
                + report.schema_drift.len()
                + report.dangling_relationships.len();
            let stats = format!("{} issue(s) in {}", issues, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        MemoryCommands::Consolidate {
            apply,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;
            let manifest = nestweaver_engine::memory_consolidate(&store, apply, now_epoch_secs())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&manifest)?);
            } else {
                println!(
                    "Consolidation ({}):",
                    if manifest.dry_run { "dry-run" } else { "apply" }
                );
                for w in &manifest.warnings {
                    println!("  warning: {w}");
                }
                if manifest.proposals.is_empty() {
                    println!("  no promotion candidates.");
                } else {
                    for p in &manifest.proposals {
                        println!("  promote {} → {}", p.source_path, p.promote_to);
                        println!("    {}", p.rationale);
                    }
                }
            }
            let stats = format!(
                "{} proposal(s) in {}",
                manifest.proposals.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        MemoryCommands::Related {
            uid,
            edge_types,
            depth,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;
            let related =
                nestweaver_engine::memory_related(&store, &uid, &edge_types, Some(depth))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&related)?);
            } else if related.is_empty() {
                println!("No typed neighbours found for {uid}.");
            } else {
                println!("Typed neighbours of {uid} ({}):", related.len());
                for r in &related {
                    println!(
                        "  [{}] {} — {} (via {})",
                        r.depth, r.title, r.file_path, r.via_edge
                    );
                }
            }
            let stats = format!(
                "{} neighbour(s) in {}",
                related.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }
    }
}

/// Try to dispatch a read-only brain command through the daemon's JSON
/// pass-through RPC. Returns `Some(json_value)` on success, `None` if the
/// daemon is unavailable or the RPC fails (caller should fall through to
/// direct-disk mode).
fn try_daemon_json_rpc(
    use_daemon: bool,
    db_path: &std::path::Path,
    config: Option<&std::path::Path>,
    rpc_name: &str,
    args: serde_json::Value,
) -> Option<serde_json::Value> {
    if !use_daemon {
        return None;
    }
    let rt = tokio::runtime::Runtime::new().ok()?;
    let mut client = rt
        .block_on(nestweaver_client::DaemonClient::connect(db_path, config))
        .ok()?;
    let args_json = serde_json::to_string(&args).ok()?;
    let req = tonic::Request::new(nestweaver_proto::JsonRequest { args_json });
    let rpc_name_owned = rpc_name.to_string();
    let result = rt.block_on(async {
        // Route to the correct RPC method by name.
        let resp = match rpc_name_owned.as_str() {
            "brain_broken_links" => client.inner_mut().brain_broken_links(req).await,
            "brain_orphan_documents" => client.inner_mut().brain_orphan_documents(req).await,
            "brain_topic_clusters" => client.inner_mut().brain_topic_clusters(req).await,
            "brain_tag_graph" => client.inner_mut().brain_tag_graph(req).await,
            "brain_doc_stats" => client.inner_mut().brain_doc_stats(req).await,
            "brain_impact" => client.inner_mut().impact(req).await,
            "regex_search" => client.inner_mut().regex_search(req).await,
            "stale_check" => client.inner_mut().stale_check(req).await,
            "brain_status" => client.inner_mut().brain_status_json(req).await,
            "read_symbols" => client.inner_mut().read_symbols(req).await,
            "dead_code" => client.inner_mut().dead_code(req).await,
            "clusters" => client.inner_mut().clusters(req).await,
            "bridge_nodes" => client.inner_mut().bridge_nodes(req).await,
            "blast_radius" => client.inner_mut().blast_radius(req).await,
            "get_summary" => client.inner_mut().get_summary(req).await,
            "count_patterns" => client.inner_mut().count_patterns(req).await,
            "affected_tests" => client.inner_mut().affected_tests(req).await,
            "investigate" => client.inner_mut().investigate(req).await,
            "investigate_expand" => client.inner_mut().investigate_expand(req).await,
            "investigate_hydrate" => client.inner_mut().investigate_hydrate(req).await,
            "flow_trace" => client.inner_mut().flow_trace(req).await,
            "contract_drift" => client.inner_mut().contract_drift(req).await,
            "cross_repo_contracts" => client.inner_mut().cross_repo_contracts(req).await,
            "brain_memory_lint" => client.inner_mut().brain_memory_lint(req).await,
            "brain_memory_consolidate" => client.inner_mut().brain_memory_consolidate(req).await,
            "brain_memory_related" => client.inner_mut().brain_memory_related(req).await,
            "detect_changes" => client.inner_mut().detect_changes(req).await,
            "brain_guide" => client.inner_mut().brain_guide(req).await,
            "list_repos" => client.inner_mut().list_repos_json(req).await,
            "list_services" => client.inner_mut().list_services_json(req).await,
            "service_summary" => client.inner_mut().service_summary_json(req).await,
            "list_projects" => client.inner_mut().list_projects_json(req).await,
            "search_symbols" => client.inner_mut().search_symbols(req).await,
            "symbol_lookup" => client.inner_mut().symbol_lookup(req).await,
            "repo_map" => client.inner_mut().repo_map_json(req).await,
            "suggest_links" => client.inner_mut().suggest_links_json(req).await,
            "detect_implicit_projects" => {
                client.inner_mut().detect_implicit_projects_json(req).await
            }
            "pr_impact" => client.inner_mut().pr_impact_json(req).await,
            _ => return None,
        };
        resp.ok().map(|r| r.into_inner().result_json)
    })?;
    serde_json::from_str(&result).ok()
}

fn run_brain(
    command: BrainCommands,
    out: &OutputConfig,
    t0: std::time::Instant,
    use_daemon: bool,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        BrainCommands::Add {
            path,
            name,
            instance,
            db,
            config,
            ignore,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let instance_cfg = load_instance_config_opt(config.as_deref());
            let instance_id_owned = instance
                .or_else(|| instance_cfg.map(|c| c.instance_id))
                .unwrap_or_else(|| "default".to_string());
            let instance_id = instance_id_owned.as_str();
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

            let extra_patterns = parse_ignore_flag(&ignore);

            if use_daemon {
                let rt = tokio::runtime::Runtime::new()?;
                let mut client =
                    rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None))?;
                let req = nestweaver_proto::IndexVaultRequest {
                    vault_path: path.display().to_string(),
                    vault_name: vault_name.clone(),
                    extra_ignore_patterns: extra_patterns.clone(),
                    instance_id: instance_id.to_string(),
                };
                rt.block_on(async {
                    let mut stream = client.inner_mut().index_vault(req).await?.into_inner();
                    while let Some(progress) = stream.message().await? {
                        let phase_name = match progress.phase {
                            0 => "Discovering",
                            1 => "Parsing",
                            2 => "Resolving",
                            3 => "Writing",
                            4 => "PageRank",
                            5 => "Done",
                            6 => "Error",
                            _ => "Unknown",
                        };
                        eprintln!("[{phase_name}] {}", progress.message);
                    }
                    Ok::<_, anyhow::Error>(())
                })?;
                return Ok((EXIT_SUCCESS, None));
            }

            // Direct-write fallback for test/CI (NESTWEAVER_NO_DAEMON=1).
            let daemon_was_running_brain = stop_daemon_if_running(&db_path);

            let result = index_markdown_directory_with_ignore(
                &path,
                &db_path,
                instance_id,
                &vault_name,
                &extra_patterns,
            )
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

            // Auto-populate Tantivy BM25 index after brain add so that
            // `brain search` works immediately without a manual reindex.
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            match TantivyIndex::open_or_create(&tantivy_path) {
                Ok(tantivy) => {
                    let store_for_tantivy = open_store(Some(&db_path))?;
                    match tantivy.reindex_from_store(&store_for_tantivy) {
                        Ok(count) => out.status(&format!("Tantivy: indexed {count} document(s)")),
                        Err(e) => tracing::warn!("Tantivy reindex failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("Tantivy open failed: {e}"),
            }

            if !result.skipped.is_empty() {
                out.status(&format!("Skipped {} file(s):", result.skipped.len()));
                for sf in &result.skipped {
                    out.status(&format!("  {} - {}", sf.path, sf.reason));
                }
            }

            // Restart daemon if we stopped it for direct-mode indexing
            if daemon_was_running_brain {
                restart_daemon(&db_path, None);
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

        BrainCommands::Status { json, db, config } => {
            let db_resolved = resolve_db_with_config(db, config.as_deref())?;
            let db_path = db_resolved.as_path();

            // ── daemon guard ──────────────────────────────────────
            if let Some(value) = try_daemon_json_rpc(
                use_daemon,
                db_path,
                config.as_deref(),
                "brain_status",
                serde_json::json!({}),
            ) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    println!("Brain status:");
                    println!("  Database:  {}", db_path.display());
                    let vault_count = value
                        .get("vault_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!("  Vaults:    {}", vault_count);
                    if let Some(vaults) = value.get("vaults").and_then(|v| v.as_array()) {
                        // When two rows share a name we annotate with the
                        // instance_id so the user can target precise removes.
                        // Empty-named rows (phantom registrations) always
                        // get annotated so they don't render as blank lines.
                        let mut name_counts: std::collections::HashMap<&str, usize> =
                            std::collections::HashMap::new();
                        for v in vaults {
                            let name = v["name"].as_str().unwrap_or("?");
                            *name_counts.entry(name).or_insert(0) += 1;
                        }
                        for v in vaults {
                            let name = v["name"].as_str().unwrap_or("?");
                            let note_count = v["note_count"].as_u64().unwrap_or(0);
                            let last_indexed = v["last_indexed"].as_str().unwrap_or("never");
                            let ambiguous = name_counts.get(name).copied().unwrap_or(0) > 1;
                            let unnamed = name.is_empty();
                            if ambiguous || unnamed {
                                let instance = v["instance_id"].as_str().unwrap_or("?");
                                let root_path = v["root_path"].as_str().unwrap_or("?");
                                let display = if unnamed {
                                    format!("<unnamed: {root_path}>")
                                } else {
                                    name.to_string()
                                };
                                println!(
                                    "    - {display} [instance: {instance}] ({note_count} notes, last indexed: {last_indexed})"
                                );
                            } else {
                                println!(
                                    "    - {name} ({note_count} notes, last indexed: {last_indexed})"
                                );
                            }
                        }
                    }
                    let notes = value.get("notes").and_then(|v| v.as_u64()).unwrap_or(0);
                    let headings = value.get("headings").and_then(|v| v.as_u64()).unwrap_or(0);
                    let sections = value.get("sections").and_then(|v| v.as_u64()).unwrap_or(0);
                    let tags = value.get("tags").and_then(|v| v.as_u64()).unwrap_or(0);
                    let wikilinks = value.get("wikilinks").and_then(|v| v.as_u64()).unwrap_or(0);
                    let repo_count = value
                        .get("repo_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!("  Notes:     {notes}");
                    println!("  Headings:  {headings}");
                    println!("  Sections:  {sections}");
                    println!("  Tags:      {tags}");
                    println!("  Wikilinks: {wikilinks}");
                    println!("  Repos:     {repo_count}");
                    // Interaction tracking — local check (not in MCP response).
                    // The sidecar is created (empty) by `InteractionTracker::new`
                    // when an MCP/daemon starts with --track-interactions, so:
                    //   - file present + scores > 0  -> "enabled, N scored"
                    //   - file present + scores == 0 -> "enabled (no events yet)"
                    //   - file absent                -> "disabled"
                    match nestweaver_engine::load_interaction_data(db_path) {
                        Some(data) if !data.scores.is_empty() => {
                            println!(
                                "  interaction_tracking: enabled ({} nodes scored)",
                                data.scores.len()
                            );
                        }
                        Some(_) => {
                            println!("  interaction_tracking: enabled (no events recorded yet)");
                        }
                        None => {
                            println!(
                                "  interaction_tracking: disabled (run with --track-interactions to enable)"
                            );
                        }
                    }
                    // Forward structured warnings from the MCP response —
                    // e.g. duplicate-vault-root collisions. Previously these
                    // were only emitted on the local (non-daemon) code path.
                    if let Some(warnings) = value.get("warnings").and_then(|v| v.as_array()) {
                        for w in warnings {
                            let kind = w["kind"].as_str().unwrap_or("");
                            if kind == "duplicate_vault_root" {
                                let root = w["root_path"].as_str().unwrap_or("?");
                                let entries = w["entries"].as_array().cloned().unwrap_or_default();
                                eprintln!(
                                    "Warning: {} vault entries share root path {}:",
                                    entries.len(),
                                    root
                                );
                                for e in &entries {
                                    let name = e["name"].as_str().unwrap_or("?");
                                    let instance = e["instance_id"].as_str().unwrap_or("?");
                                    let uid = e["uid"].as_str().unwrap_or("?");
                                    let n = e["note_count"].as_u64().unwrap_or(0);
                                    eprintln!(
                                        "    - {name} [instance: {instance}] uid={uid} ({n} notes)"
                                    );
                                }
                                eprintln!(
                                    "  This usually means brain add was run with different --instance values."
                                );
                                // Prefer the targeted remediation produced
                                // by tool_brain_status when present, fall
                                // back to the generic three-option guidance
                                // for older daemon binaries.
                                let cmds = w["remediation_commands"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default();
                                let hint = w["remediation_hint"].as_str().unwrap_or("");
                                if !cmds.is_empty() {
                                    if !hint.is_empty() {
                                        eprintln!("  {hint}");
                                    }
                                    eprintln!("  Run:");
                                    for c in &cmds {
                                        if let Some(s) = c.as_str() {
                                            eprintln!("      {s}");
                                        }
                                    }
                                } else {
                                    eprintln!(
                                        "  Fix one row precisely with:\n      nestweaver brain remove --instance <instance-id>\n  \
                                         Or sweep all rows at this path:\n      nestweaver brain remove {root}\n  \
                                         Or consolidate under one instance:\n      nestweaver instance merge --from <old-id> --to <correct-id>"
                                    );
                                }
                            }
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

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
                // Match the canonical schema emitted by `tool_brain_status`
                // so daemon-routed and local (`--no-daemon`) callers parse
                // identical shapes:
                //   - `vaults` is an array of vault detail objects.
                //   - `vault_count` is the total count.
                //   - `repos` is an array of repo objects; `repo_count` the total.
                // `vault_details` is preserved as an alias for `vaults` so
                // scripts written against the previous local-only shape keep
                // working through the deprecation window.
                let vault_details: Vec<serde_json::Value> = vaults
                    .iter()
                    .map(|v| {
                        let vault_note_count =
                            store.list_notes(Some(&v.uid)).unwrap_or_default().len();
                        let last_indexed = resolve_last_indexed(db_path, &v.uid, &store);
                        serde_json::json!({
                            "uid": v.uid,
                            "instance_id": v.instance_id,
                            "name": v.name,
                            "root_path": v.root_path,
                            "note_count": vault_note_count,
                            "last_indexed": last_indexed,
                        })
                    })
                    .collect();
                let repo_details: Vec<serde_json::Value> = repos
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "url": r.url,
                            "sha": r.indexed_sha,
                            "instance_id": r.instance_id,
                            "name": r.name,
                        })
                    })
                    .collect();
                let mut instance_ids: std::collections::BTreeSet<&str> =
                    vaults.iter().map(|v| v.instance_id.as_str()).collect();
                instance_ids.extend(repos.iter().map(|r| r.instance_id.as_str()));
                let instance_ids_json: Vec<String> =
                    instance_ids.into_iter().map(|s| s.to_string()).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "db": db_path.display().to_string(),
                        "vaults": vault_details,
                        "vault_count": vaults.len(),
                        "vault_details": vault_details,
                        "notes": note_count,
                        "headings": heading_count,
                        "sections": section_count,
                        "tags": tag_count,
                        "wikilinks": wikilink_count,
                        "repos": repo_details,
                        "repo_count": repos.len(),
                        "instance_ids": instance_ids_json,
                    }))?
                );
            } else {
                println!("Brain status:");
                println!("  Database:  {}", db_path.display());
                println!("  Vaults:    {}", vaults.len());
                // When two rows share a name + root_path we surface
                // `instance_id` so the user can tell them apart and target
                // `brain remove --instance <id>` precisely. Empty-named
                // rows (phantom registrations) are always annotated so they
                // don't render as blank lines.
                let mut name_counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for v in &vaults {
                    *name_counts.entry(v.name.as_str()).or_insert(0) += 1;
                }
                for v in &vaults {
                    let vault_note_count = store.list_notes(Some(&v.uid)).unwrap_or_default().len();
                    let last_indexed = resolve_last_indexed(db_path, &v.uid, &store)
                        .unwrap_or_else(|| "never".to_string());
                    let ambiguous = name_counts.get(v.name.as_str()).copied().unwrap_or(0) > 1;
                    let unnamed = v.name.is_empty();
                    if ambiguous || unnamed {
                        let display = if unnamed {
                            format!("<unnamed: {}>", v.root_path)
                        } else {
                            v.name.clone()
                        };
                        println!(
                            "    - {display} [instance: {}] ({vault_note_count} notes, last indexed: {last_indexed})",
                            v.instance_id
                        );
                    } else {
                        println!(
                            "    - {} ({vault_note_count} notes, last indexed: {last_indexed})",
                            v.name
                        );
                    }
                }
                println!("  Notes:     {note_count}");
                println!("  Headings:  {heading_count}");
                println!("  Sections:  {section_count}");
                println!("  Tags:      {tag_count}");
                println!("  Wikilinks: {wikilink_count}");
                println!("  Repos:     {}", repos.len());
                // Interaction tracking is opt-in (enabled via `mcp
                // --track-interactions`). InteractionTracker::new touches
                // the sidecar at startup so we can distinguish three states:
                //   - file present + scores > 0  -> enabled, accumulating
                //   - file present + scores == 0 -> enabled, no events yet
                //   - file absent                -> disabled
                match nestweaver_engine::load_interaction_data(db_path) {
                    Some(data) if !data.scores.is_empty() => {
                        println!(
                            "  interaction_tracking: enabled ({} nodes scored)",
                            data.scores.len()
                        );
                    }
                    Some(_) => {
                        println!("  interaction_tracking: enabled (no events recorded yet)");
                    }
                    None => {
                        println!(
                            "  interaction_tracking: disabled (run with --track-interactions to enable)"
                        );
                    }
                }
            }

            // Warn when multiple vault UIDs map to the same canonical root
            // path — usually caused by brain add with mismatched --instance
            // or missing --config. This produces phantom 0-note vault rows.
            // Each entry surfaces name + instance_id + uid so the user can
            // target `brain remove --instance <id>` precisely.
            let mut root_to_vaults: std::collections::HashMap<
                &str,
                Vec<&nestweaver_schema::Vault>,
            > = std::collections::HashMap::new();
            for v in &vaults {
                root_to_vaults
                    .entry(v.root_path.as_str())
                    .or_default()
                    .push(v);
            }
            for (root, rows) in &root_to_vaults {
                if rows.len() > 1 {
                    eprintln!(
                        "Warning: {} vault entries share root path {}:",
                        rows.len(),
                        root
                    );
                    for v in rows {
                        let n = store.list_notes(Some(&v.uid)).unwrap_or_default().len();
                        eprintln!(
                            "    - {} [instance: {}] uid={} ({} notes)",
                            v.name, v.instance_id, v.uid, n
                        );
                    }
                    eprintln!(
                        "  This usually means brain add was run with different --instance values."
                    );
                    eprintln!(
                        "  Fix one row precisely with:\n      nestweaver brain remove --instance <instance-id>\n  \
                         Or sweep all rows at this path:\n      nestweaver brain remove {root}\n  \
                         Or consolidate under one instance:\n      nestweaver instance merge --from <old-id> --to <correct-id>",
                    );
                }
            }

            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::StaleCheck { json, db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            // ── daemon guard ──────────────────────────────────────
            if let Some(value) = try_daemon_json_rpc(
                use_daemon,
                &db_path,
                None,
                "stale_check",
                serde_json::json!({}),
            ) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    let repo_count = value
                        .get("repo_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let any_stale = value
                        .get("any_stale")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let repos = value
                        .get("repos")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    if repos.is_empty() {
                        println!("No repos indexed.");
                    } else {
                        println!(
                            "Stale check: {} repo(s), {}",
                            repo_count,
                            if any_stale {
                                "INDEX IS STALE"
                            } else {
                                "up to date"
                            }
                        );
                        for r in &repos {
                            let url = r["url"].as_str().unwrap_or("?");
                            let stale = r["is_stale"].as_bool().unwrap_or(false);
                            let indexed_full = r["indexed_sha"].as_str().unwrap_or("?");
                            let indexed = &indexed_full[..8.min(indexed_full.len())];
                            let head = r["current_head"]
                                .as_str()
                                .map(|h| &h[..8.min(h.len())])
                                .unwrap_or("unknown");
                            let behind = r["staleness_commits_behind"].as_u64().unwrap_or(0);
                            let marker = if stale { "STALE" } else { "ok" };
                            if stale && behind > 0 {
                                println!(
                                    "  [{marker}] {url}  indexed={indexed}  HEAD={head}  ({behind} commits behind)"
                                );
                            } else {
                                println!("  [{marker}] {url}  indexed={indexed}  HEAD={head}");
                            }
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let repos = store.list_repos(None).unwrap_or_default();

            let mut any_stale = false;
            let mut results: Vec<serde_json::Value> = Vec::new();

            for repo in &repos {
                let current_head = if let Some(path) = repo.url.strip_prefix("file://") {
                    std::process::Command::new("git")
                        .args(["-C", path, "rev-parse", "HEAD"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                };

                let is_valid_sha = repo.indexed_sha.len() == 40
                    && repo.indexed_sha.chars().all(|c| c.is_ascii_hexdigit());
                let commits_behind = match (&current_head, repo.url.strip_prefix("file://")) {
                    (Some(head), Some(path)) if is_valid_sha && *head != repo.indexed_sha => {
                        std::process::Command::new("git")
                            .args([
                                "-C",
                                path,
                                "rev-list",
                                "--count",
                                &format!("{}..{}", repo.indexed_sha, head),
                            ])
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .and_then(|o| {
                                String::from_utf8_lossy(&o.stdout)
                                    .trim()
                                    .parse::<u64>()
                                    .ok()
                            })
                            .unwrap_or(0)
                    }
                    _ => repo.staleness_commits_behind as u64,
                };
                let is_stale = match &current_head {
                    Some(head) => head != &repo.indexed_sha,
                    None => commits_behind > 0,
                };
                if is_stale {
                    any_stale = true;
                }

                results.push(serde_json::json!({
                    "url": repo.url,
                    "indexed_sha": repo.indexed_sha,
                    "current_head": current_head,
                    "is_stale": is_stale,
                    "staleness_commits_behind": commits_behind,
                }));
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "repo_count": repos.len(),
                        "any_stale": any_stale,
                        "repos": results,
                    }))?
                );
            } else if repos.is_empty() {
                println!("No repos indexed.");
            } else {
                println!(
                    "Stale check: {} repo(s), {}",
                    repos.len(),
                    if any_stale {
                        "INDEX IS STALE"
                    } else {
                        "up to date"
                    }
                );
                for r in &results {
                    let url = r["url"].as_str().unwrap_or("?");
                    let stale = r["is_stale"].as_bool().unwrap_or(false);
                    let indexed_full = r["indexed_sha"].as_str().unwrap_or("?");
                    let indexed = &indexed_full[..8.min(indexed_full.len())];
                    let head = r["current_head"]
                        .as_str()
                        .map(|h| &h[..8.min(h.len())])
                        .unwrap_or("unknown");
                    let behind = r["staleness_commits_behind"].as_u64().unwrap_or(0);
                    let marker = if stale { "STALE" } else { "ok" };
                    if stale && behind > 0 {
                        println!(
                            "  [{marker}] {url}  indexed={indexed}  HEAD={head}  ({behind} commits behind)"
                        );
                    } else {
                        println!("  [{marker}] {url}  indexed={indexed}  HEAD={head}");
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Watch {
            path,
            name,
            instance,
            db,
            ignore,
            refresh_wiki_hours,
            config,
        } => {
            if refresh_wiki_hours.is_some() && config.is_none() {
                eprintln!("Error: --refresh-wiki-hours requires --config");
                return Ok((EXIT_ERROR, None));
            }
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
            // Instance ID priority: --instance flag > config's instance_id > "default"
            let instance_cfg = load_instance_config_opt(config.as_deref());
            let instance_id = instance.unwrap_or_else(|| {
                instance_cfg
                    .as_ref()
                    .map(|c| c.instance_id.clone())
                    .unwrap_or_else(|| "default".to_string())
            });

            if let Some(hours) = refresh_wiki_hours {
                out.status(&format!(
                    "Wiki refresh scheduled every {}h (via materialize-instance)",
                    hours
                ));
            }

            // Respect watch config when --config is provided.
            let watch_cfg = instance_cfg.map(|c| c.watch).unwrap_or_default();
            if !watch_cfg.enabled {
                out.status(
                    "Watching disabled in instance config ([watch] enabled = false). Exiting.",
                );
                return Ok((EXIT_SUCCESS, None));
            }

            let extra_patterns = parse_ignore_flag(&ignore);

            if use_daemon {
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ))?;
                let req = nestweaver_proto::WatchVaultRequest {
                    vault_path: path.display().to_string(),
                    vault_name: vault_name.clone(),
                    instance_id: instance_id.clone(),
                    extra_ignore_patterns: extra_patterns.clone(),
                };
                let resp = rt.block_on(async {
                    client
                        .inner_mut()
                        .watch_vault(req)
                        .await
                        .map(|r| r.into_inner())
                })?;
                if !resp.ok {
                    eprintln!("Error: {}", resp.message);
                    return Ok((EXIT_ERROR, None));
                }
                out.status(&format!(
                    "Watching {} via daemon (Ctrl-C to stop)",
                    path.display()
                ));

                // Block until Ctrl-C or daemon death, then send StopWatch.
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = ctrlc_handler(move || {
                    let _ = tx.send(());
                });

                // Periodic health check so we notice daemon death.
                loop {
                    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let health = rt.block_on(async {
                                client
                                    .inner_mut()
                                    .health_check(nestweaver_proto::HealthCheckRequest {})
                                    .await
                            });
                            if health.is_err() {
                                eprintln!("Daemon is no longer running.");
                                return Ok((EXIT_ERROR, None));
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                let stop_req = nestweaver_proto::StopWatchRequest {};
                if let Err(e) = rt.block_on(async { client.inner_mut().stop_watch(stop_req).await })
                {
                    // The watcher thread lives inside the daemon, so when the
                    // daemon process is gone the watcher has already stopped
                    // with it - nothing for us to clean up, nothing to warn
                    // about. Two error shapes indicate "daemon gone":
                    //   - `Unavailable`: connect failed (socket file removed,
                    //     or "Connection refused" on a stale socket).
                    //   - `Unknown` with a tonic transport error: the
                    //     connection was open when the RPC started but was
                    //     abruptly closed mid-call (e.g. daemon SIGKILLed).
                    // Without this filter, `KeepAlive=true` on the watch
                    // plist turns every daemon restart into a perpetual
                    // "failed to stop watcher" loop in the watch error log.
                    let daemon_gone = matches!(e.code(), tonic::Code::Unavailable)
                        || (matches!(e.code(), tonic::Code::Unknown)
                            && e.message().contains("transport error"));
                    if !daemon_gone {
                        eprintln!("Warning: failed to stop watcher: {e}");
                    }
                }
                out.status("Watcher stopped.");
                return Ok((EXIT_SUCCESS, None));
            }

            let tantivy_sidecar = tantivy_sidecar_path_for(&db_path);
            let manifests_path = db_path.with_extension("manifests.json");
            let wiki_instance_id = instance_id.clone();
            let watcher = BrainWatcher::new(&db_path, &path, instance_id, vault_name)
                .with_tantivy_index(&tantivy_sidecar)
                .with_manifests_path(&manifests_path)
                .with_extra_ignore_patterns(&extra_patterns)
                .with_debounce_ms(watch_cfg.debounce_ms);
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

            // Spawn periodic wiki refresh thread if --refresh-wiki-hours
            // is set. The thread sleeps for N hours, calls
            // materialize_projects to re-fetch wiki sources, then loops
            // until the shutdown handle signals stop.
            if let (Some(hours), Some(config_path)) = (refresh_wiki_hours, config.as_deref()) {
                let wiki_db = db_path.clone();
                let wiki_config_path = config_path.to_path_buf();
                let wiki_stop = stop.clone();
                let wiki_instance = wiki_instance_id;
                std::thread::spawn(move || {
                    let interval = std::time::Duration::from_secs(hours * 3600);
                    loop {
                        // Sleep in small increments so we notice shutdown quickly.
                        let deadline = std::time::Instant::now() + interval;
                        while std::time::Instant::now() < deadline {
                            if wiki_stop.is_stopped() {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(5));
                        }
                        if wiki_stop.is_stopped() {
                            return;
                        }
                        tracing::info!("periodic wiki refresh triggered");
                        match nestweaver_engine::InstanceConfig::from_file(&wiki_config_path) {
                            Ok(cfg) => {
                                let store = match GraphStore::open_or_create(&wiki_db) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        tracing::warn!("wiki refresh: failed to open store: {e}");
                                        continue;
                                    }
                                };
                                match materialize_projects(&store, &cfg, &wiki_instance, &wiki_db) {
                                    Ok(res) => tracing::info!(
                                        projects = res.projects_created,
                                        wiki_notes = res.wiki_notes_ingested,
                                        "wiki refresh complete"
                                    ),
                                    Err(e) => tracing::warn!("wiki refresh failed: {e}"),
                                }
                            }
                            Err(e) => tracing::warn!("wiki refresh: config load failed: {e}"),
                        }
                    }
                });
            }

            out.status(&format!(
                "Watching {} -> {} (Ctrl-C to stop)",
                path.display(),
                db_path.display()
            ));
            watcher.run().context("watcher")?;

            // BrainWatcher::run() drops its GraphStore when it returns,
            // which triggers lbug's internal cleanup. However, `launchctl
            // unload` sends SIGKILL after a short grace period (~5 s) if
            // the process hasn't exited. A small sleep here gives the OS
            // time to flush any remaining WAL pages to disk after the
            // store is dropped inside `run()`.
            std::thread::sleep(std::time::Duration::from_millis(100));

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
            ignore,
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
            let extra_patterns = parse_ignore_flag(&ignore);

            // Compute vault UID for recording last_indexed_at.
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let v_uid = nestweaver_schema::vault_uid(&instance_id, &canonical.to_string_lossy());

            if use_daemon && since.is_none() {
                // Route full refresh through daemon's IndexVault RPC
                let rt = tokio::runtime::Runtime::new()?;
                let mut client =
                    rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None))?;
                let req = nestweaver_proto::IndexVaultRequest {
                    vault_path: path.display().to_string(),
                    vault_name: vault_name.clone(),
                    extra_ignore_patterns: extra_patterns.clone(),
                    instance_id: instance_id.to_string(),
                };
                rt.block_on(async {
                    let mut stream = client.inner_mut().index_vault(req).await?.into_inner();
                    while let Some(progress) = stream.message().await? {
                        let phase_name = match progress.phase {
                            5 => "Done",
                            6 => "Error",
                            _ => "Progress",
                        };
                        eprintln!("[{phase_name}] {}", progress.message);
                    }
                    Ok::<_, anyhow::Error>(())
                })?;
                return Ok((EXIT_SUCCESS, None));
            }

            if let Some(since_str) = since {
                // Incremental refresh: only re-index files modified since the
                // given timestamp.
                let since_time = parse_iso8601_to_system_time(&since_str).with_context(|| {
                    format!(
                        "invalid --since timestamp '{}': expected ISO 8601 (e.g. 2026-05-26T00:00:00Z)",
                        since_str
                    )
                })?;
                let result = index_markdown_directory_since_with_ignore(
                    &path,
                    &db_path,
                    &instance_id,
                    &vault_name,
                    since_time,
                    &extra_patterns,
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

                let result = index_markdown_directory_with_ignore(
                    &path,
                    &db_path,
                    &instance_id,
                    &vault_name,
                    &extra_patterns,
                )
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

            // Auto-populate Tantivy BM25 index after brain refresh so that
            // `brain search` works immediately without a manual reindex.
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            match TantivyIndex::open_or_create(&tantivy_path) {
                Ok(tantivy) => {
                    let store_for_tantivy = open_store(Some(&db_path))?;
                    match tantivy.reindex_from_store(&store_for_tantivy) {
                        Ok(count) => {
                            println!("Tantivy: indexed {count} document(s)");
                        }
                        Err(e) => tracing::warn!("Tantivy reindex failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("Tantivy open failed: {e}"),
            }

            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Remove { path, instance, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let instance_specified = instance.is_some();
            let instance_id = instance.as_deref().unwrap_or("default");

            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let canon_str = canonical.to_string_lossy();
            let raw_str = path.to_string_lossy();
            let v_uid_canon = nestweaver_schema::vault_uid(instance_id, &canon_str);
            let v_uid_raw = nestweaver_schema::vault_uid(instance_id, &raw_str);

            let store = GraphStore::open_read_only(&db_path)
                .with_context(|| format!("failed to open database at {}", db_path.display()))?;

            // Helper: a stored vault matches the caller's path if any of its
            // representations (canonical, literal, shell-expanded `~`)
            // resolve to the same absolute path. `brain add` may have
            // registered the vault with a literal `~/...` string (from a
            // config file or programmatic call) while the caller of
            // `brain remove` typically passes a shell-expanded absolute
            // path. A naive `vault_uid` lookup misses these cases even
            // though `brain status` clearly shows the row.
            let home = std::env::var("HOME").ok();
            let path_matches = |stored: &str| -> bool {
                if stored == canon_str || stored == raw_str {
                    return true;
                }
                let stored_canon = std::fs::canonicalize(stored)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| stored.to_string());
                if stored_canon == *canon_str {
                    return true;
                }
                if let (Some(h), Some(rest)) = (home.as_deref(), stored.strip_prefix("~/")) {
                    let expanded = format!("{h}/{rest}");
                    if expanded == *canon_str {
                        return true;
                    }
                    if let Ok(c) = std::fs::canonicalize(&expanded)
                        && c.to_string_lossy() == *canon_str
                    {
                        return true;
                    }
                }
                false
            };

            // If the caller passed `--instance`, treat it as a precise
            // selector. If the direct vault_uid lookup misses (path
            // stored under a non-canonical form like a literal `~/...`),
            // fall back to a list-scan scoped to the requested instance
            // before failing.
            //
            // If `--instance` is absent, fall back to the historical
            // ghost-row cleanup behavior: remove the default-UID row
            // plus any other row whose canonical root_path matches.
            let mut uids_to_remove: Vec<String> = Vec::new();
            if instance_specified {
                if store.lookup_vault(&v_uid_canon).is_ok() {
                    uids_to_remove.push(v_uid_canon);
                } else if store.lookup_vault(&v_uid_raw).is_ok() {
                    uids_to_remove.push(v_uid_raw);
                } else if let Ok(all_vaults) = store.list_vaults(Some(instance_id)) {
                    for v in &all_vaults {
                        if path_matches(&v.root_path) {
                            uids_to_remove.push(v.uid.clone());
                        }
                    }
                }
                if uids_to_remove.is_empty() {
                    eprintln!(
                        "Error: no vault with instance '{instance_id}' found at {canon_str}.\n  \
                         Run `nestweaver brain status` to see registered vaults and their instance ids,\n  \
                         then re-run with the correct --instance (or omit --instance to clean up every row at this path)."
                    );
                    return Ok((EXIT_NOT_FOUND, None));
                }
            } else {
                uids_to_remove.push(v_uid_canon);
                if let Ok(all_vaults) = store.list_vaults(None) {
                    for v in &all_vaults {
                        if path_matches(&v.root_path) && !uids_to_remove.contains(&v.uid) {
                            uids_to_remove.push(v.uid.clone());
                        }
                    }
                }
            }

            let mut vault_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("vault")
                .to_string();
            for uid in &uids_to_remove {
                if let Ok(v) = store.lookup_vault(uid) {
                    vault_name = v.name;
                }
            }

            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            let mut total_dropped = 0usize;
            let mut rows_cleaned = 0usize;
            for uid in &uids_to_remove {
                match rt.block_on(client.remove_vault(uid)) {
                    Ok(resp) => {
                        total_dropped += resp.notes_deleted as usize;
                        rows_cleaned += 1;
                    }
                    Err(e) => {
                        eprintln!("Error: failed to remove vault '{uid}': {e}.");
                        return Ok((EXIT_ERROR, None));
                    }
                }
            }
            println!(
                "Removed vault '{}' ({} note(s) dropped, {} row(s) cleaned). \
                 Tantivy + PPR sidecars may be stale; run \
                 `nestweaver brain reindex-search` if you want to clear them too.",
                vault_name, total_dropped, rows_cleaned
            );
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::ReindexSearch { db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            if use_daemon && let Ok(rt) = tokio::runtime::Runtime::new() {
                let connect = rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None));
                if let Ok(mut client) = connect {
                    let rpc = rt.block_on(async {
                        client
                            .inner_mut()
                            .reindex_search(nestweaver_proto::ReindexSearchRequest {})
                            .await
                            .map(|r| r.into_inner())
                    });
                    match rpc {
                        Ok(resp) => {
                            let sidecar = tantivy_sidecar_path_for(&db_path);
                            println!(
                                "Tantivy reindex complete: {} document(s) at {} (via daemon)",
                                resp.document_count,
                                sidecar.display()
                            );
                            return Ok((EXIT_SUCCESS, None));
                        }
                        Err(status) => {
                            eprintln!(
                                "warning: daemon reindex RPC failed ({}); falling back to direct mode",
                                status.message()
                            );
                        }
                    }
                }
            }

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
            query: raw_query,
            limit,
            json,
            db,
            config,
            prf,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 20);

            // Route through the daemon's typed `Search` RPC when running. The
            // daemon owns the writer-mode Tantivy index and shares dispatch
            // with the MCP server (`tool_brain_search`), so daemon-routed
            // searches eliminate the "Database is locked" reader fallback
            // and stay in sync with live re-indexing. Falls through to the
            // direct-disk implementation below when `--no-daemon` is set,
            // `NESTWEAVER_NO_DAEMON` is in the env, or the daemon is down.
            if use_daemon && let Ok(rt) = tokio::runtime::Runtime::new() {
                let connect = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ));
                if let Ok(mut client) = connect {
                    let req = nestweaver_proto::BrainSearchRequest {
                        query: raw_query.clone(),
                        limit: limit as i32,
                        response_format: None,
                        include_bodies: false,
                        prf,
                        rerank: false,
                        root: None,
                    };
                    let rpc = rt.block_on(async {
                        client.inner_mut().search(req).await.map(|r| r.into_inner())
                    });
                    match rpc {
                        Ok(resp) => {
                            render_brain_search_response(&resp, json)?;
                            let stats = format!(
                                "{} results in {} (via daemon)",
                                resp.results.len(),
                                format_elapsed(t0.elapsed())
                            );
                            return Ok((EXIT_SUCCESS, Some(stats)));
                        }
                        Err(status) => {
                            // Surface a single eprintln, then fall through to
                            // the direct-disk path so the user still gets a
                            // result.
                            eprintln!(
                                "warning: daemon search RPC failed ({}); falling back to direct DB read",
                                status.message()
                            );
                        }
                    }
                }
                // Connect failed → silently fall through (daemon may not be
                // running for this DB; the direct-disk path is the legacy
                // behavior and remains correct).
            }

            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // Parse the instance config once and reuse for ranking priors and
            // the Feature F7 `[ranking] enable_prf` default.
            let instance_cfg = load_instance_config_opt(config.as_deref());
            // Feature F6: per-path ranking priors from `[ranking]`. None → no-op.
            let ranking_config = instance_cfg
                .as_ref()
                .map(|c| c.ranking.clone())
                .filter(|r| !r.is_empty());
            // Feature F7: PRF is enabled by the --prf flag OR `[ranking] enable_prf`.
            let prf_enabled = prf
                || instance_cfg
                    .as_ref()
                    .map(|c| c.ranking.enable_prf)
                    .unwrap_or(false);

            // Expand the query with taxonomy aliases for better recall.
            let aliases = load_alias_sidecar(&db_path);
            let query = expand_query_with_aliases(&raw_query, &aliases);

            // ── Vault note results ──────────────────────────────────────
            let mut expansion_terms: Vec<String> = Vec::new();
            let (note_results, engine) = if let Some(ref idx) = tantivy {
                let raw_limit = limit * 5;
                let hits = if prf_enabled {
                    let (hits, terms) = idx
                        .search_prf(
                            &query,
                            raw_limit,
                            nestweaver_engine::query::nestweaver_store_stoplist(),
                        )
                        .with_context(|| "tantivy prf search")?;
                    expansion_terms = terms;
                    hits
                } else {
                    idx.search(&query, raw_limit)
                        .with_context(|| "tantivy search")?
                };
                let grouped = group_bm25_hits_by_note(&store, &hits, limit);
                (grouped, "bm25")
            } else {
                let needle = query.to_lowercase();
                let notes = store.list_notes(None).context("list_notes")?;
                let matches: Vec<NoteSearchResult> = notes
                    .iter()
                    .filter(|n| n.title.to_lowercase().contains(&needle))
                    .take(limit)
                    .map(|n| NoteSearchResult {
                        note_uid: n.uid.clone(),
                        title: n.title.clone(),
                        best_score: 1.0,
                        matched_headings: Vec::new(),
                    })
                    .collect();
                (matches, "substring")
            };

            // ── Code symbol results ─────────────────────────────────────
            let seen_titles: std::collections::HashSet<String> = note_results
                .iter()
                .map(|r| r.title.to_lowercase())
                .collect();

            let code_results = search_symbols(&store, &query, limit).unwrap_or_default();
            let code_results: Vec<_> = code_results
                .into_iter()
                .filter(|sym| !seen_titles.contains(&sym.name.to_lowercase()))
                .collect();

            // Feature F6: apply per-path ranking priors as a multiplier on each
            // result's relevance, keyed by file-path glob. Reuse the engine
            // helper by projecting results into BrainNodes, then map the
            // adjusted relevance back by UID. No config → empty map (no-op).
            let mut note_results = note_results;
            let prior_scores: std::collections::HashMap<String, f64> =
                if let Some(ref ranking) = ranking_config {
                    let mut probe: Vec<nestweaver_engine::BrainNode> = Vec::new();
                    for n in &note_results {
                        let location = store
                            .lookup_note(&n.note_uid)
                            .map(|note| note.file_path)
                            .unwrap_or_default();
                        probe.push(nestweaver_engine::BrainNode {
                            uid: n.note_uid.clone(),
                            kind: "Note".to_string(),
                            title: n.title.clone(),
                            location,
                            relevance: n.best_score as f64,
                            inline_body: None,
                            body_complete: true,
                        });
                    }
                    for sym in &code_results {
                        probe.push(nestweaver_engine::BrainNode {
                            uid: sym.uid.clone(),
                            kind: format!("Symbol/{}", sym.kind),
                            title: sym.name.clone(),
                            location: format!("{}:{}", sym.file_path, sym.start_line),
                            relevance: 0.5,
                            inline_body: None,
                            body_complete: true,
                        });
                    }
                    nestweaver_engine::apply_ranking_priors(&mut probe, ranking);
                    probe.into_iter().map(|n| (n.uid, n.relevance)).collect()
                } else {
                    std::collections::HashMap::new()
                };
            // Fold adjusted note scores back in.
            for n in &mut note_results {
                if let Some(&adj) = prior_scores.get(&n.note_uid) {
                    n.best_score = adj as f32;
                }
            }
            // Symbol display score: prior-adjusted when present, else 0.5.
            let code_score = |uid: &str| prior_scores.get(uid).copied().unwrap_or(0.5);

            let result_count = note_results.len() + code_results.len();

            if json {
                let mut results: Vec<serde_json::Value> = note_results
                    .iter()
                    .map(|g| {
                        let mut v = serde_json::json!({
                            "uid": g.note_uid,
                            "kind": "note",
                            "title": g.title,
                            "score": g.best_score,
                        });
                        if !g.matched_headings.is_empty() {
                            v["matched_headings"] = serde_json::json!(g.matched_headings);
                        }
                        v
                    })
                    .collect();
                for sym in &code_results {
                    results.push(serde_json::json!({
                        "uid": sym.uid,
                        "kind": format!("Symbol/{}", sym.kind),
                        "title": sym.name,
                        "score": code_score(&sym.uid),
                        "location": format!("{}:{}", sym.file_path, sym.start_line),
                    }));
                }
                // Sort by score descending. `limit` is interpreted per-kind
                // (each of notes/symbols is already capped upstream); a
                // cross-kind truncate here would evict every symbol whenever
                // ≥ `limit` notes match because symbols carry a fixed 0.5
                // score while BM25 notes score 15+. Mirrors the daemon-side
                // `tool_brain_search` semantics.
                results.sort_by(|a, b| {
                    let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut payload = serde_json::json!({
                    "query": query,
                    "engine": engine,
                    "results": results,
                    "total_matches": results.len(),
                });
                // Feature F7: surface PRF-mined expansion terms for auditing.
                if !expansion_terms.is_empty() {
                    payload["expansion_terms"] = serde_json::json!(expansion_terms);
                }
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else if note_results.is_empty() && code_results.is_empty() {
                println!("No results for '{query}'.");
            } else {
                let header = if engine == "bm25" {
                    "Brain search (BM25)"
                } else {
                    "Brain search (substring fallback)"
                };
                println!(
                    "{}: {} result(s)",
                    header,
                    note_results.len() + code_results.len()
                );
                // Feature F7: show PRF-mined expansion terms for auditing.
                if !expansion_terms.is_empty() {
                    println!("  PRF expansion terms: {}", expansion_terms.join(", "));
                }
                println!();
                for g in &note_results {
                    if g.matched_headings.is_empty() {
                        println!("  [{:.2}] {}", g.best_score, g.title);
                    } else {
                        println!(
                            "  [{:.2}] {} (matched: {})",
                            g.best_score,
                            g.title,
                            g.matched_headings.join(", "),
                        );
                    }
                }
                for sym in &code_results {
                    println!(
                        "  [{:.2}] {} [{}] @ {}:{}",
                        code_score(&sym.uid),
                        sym.name,
                        sym.kind,
                        sym.file_path,
                        sym.start_line,
                    );
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
            config: config_path,
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
            inline_bodies,
            root,
            prf,
            rerank,
            intent,
            no_tests,
            prefer_instance,
        } => {
            let db_path = resolve_db_with_config(db, config_path.as_deref())?;
            let cfg = load_instance_config_opt(config_path.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 30);

            // Parse the optional --intent override into a `QueryIntent`.
            // Surface invalid values as a CLI error rather than silently
            // ignoring (mirrors `nestweaver context --intent`).
            let parsed_intent: Option<QueryIntent> = intent
                .as_deref()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid --intent value: {e}"))?;

            // Route through daemon's GetContext RPC when available and no
            // flags require direct-disk processing. `--no-tests` and
            // `--prefer-instance` are applied client-side and the daemon
            // proto does not yet carry them, so fall through to the local
            // path when either is set.
            if use_daemon
                && !no_tests
                && prefer_instance.is_none()
                && let Ok(rt) = tokio::runtime::Runtime::new()
            {
                let connect = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config_path.as_deref(),
                ));
                if let Ok(mut client) = connect {
                    let req = nestweaver_proto::BrainContextRequest {
                        seeds: seeds.clone(),
                        token_budget: token_budget.unwrap_or(0) as i32,
                        response_format: String::new(),
                        repos: repos.clone(),
                        vaults: vaults.clone(),
                        kinds: kinds.clone(),
                        path_prefix: path_prefix.clone().unwrap_or_default(),
                        tags: tags.clone(),
                        exclude_tags: exclude_tags.clone(),
                        weight_ppr: weight_ppr.unwrap_or(0.0),
                        weight_bm25: weight_bm25.unwrap_or(0.0),
                        // Pass the parsed --intent through to the daemon
                        // (empty string = auto-detect on the server).
                        intent: intent.clone().unwrap_or_default(),
                        include_seeds: true,
                        include_bodies: inline_bodies,
                        root: root
                            .clone()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string(),
                        prf,
                        rerank,
                        weight_semantic: weight_semantic.unwrap_or(0.0),
                        since: since.as_deref().unwrap_or("").to_string(),
                        recency_weight,
                        recency_half_life_days,
                    };
                    let rpc = rt.block_on(async {
                        client
                            .inner_mut()
                            .get_context(req)
                            .await
                            .map(|r| r.into_inner())
                    });
                    match rpc {
                        Ok(resp) => {
                            let result: nestweaver_engine::BrainContextResult =
                                serde_json::from_str(&resp.result_json)?;
                            let cut = match token_budget {
                                Some(budget) => token_budgeted_truncate(&result.connected, budget),
                                None => limit.min(result.connected.len()),
                            };
                            if json {
                                print_brain_context_json(&result, cut)?;
                            } else {
                                print_brain_context_text(&result, cut, token_budget);
                            }
                            let node_count = result.seeds.len() + cut;
                            let stats = format!(
                                "{} nodes in {} (via daemon)",
                                node_count,
                                format_elapsed(t0.elapsed())
                            );
                            return Ok((EXIT_SUCCESS, Some(stats)));
                        }
                        Err(status) => {
                            eprintln!(
                                "warning: daemon context RPC failed ({}); falling back to direct DB read",
                                status.message()
                            );
                        }
                    }
                }
            }

            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // Parse the instance config once (when supplied) and reuse it for
            // both Feature F8 ([response]) and Feature F6 ([ranking]).
            let instance_cfg = load_instance_config_opt(config_path.as_deref());
            // Feature F8: response tuning comes from [response] in the instance
            // config when one is supplied; otherwise the built-in defaults.
            let response_config = instance_cfg
                .as_ref()
                .map(|c| c.response.clone())
                .unwrap_or_default();
            // Feature F6: per-path ranking priors. None → no-op below.
            let ranking_config = instance_cfg
                .as_ref()
                .map(|c| c.ranking.clone())
                .filter(|r| !r.is_empty());

            // Feature F7: PRF is enabled by the --prf flag OR `[ranking] enable_prf`.
            let prf_enabled = prf
                || instance_cfg
                    .as_ref()
                    .map(|c| c.ranking.enable_prf)
                    .unwrap_or(false);

            // RFC #6: build custom HybridSearchConfig from optional CLI flags.
            // Finding #7: thread `[seed_resolution]` (with backward-compat
            // shim for legacy `[ranking].test_path_patterns`) from the
            // instance config into the search config so user overrides reach
            // `search_symbols_by_name` at seed resolution.
            let defaults = HybridSearchConfig::default();
            let configured_seed_resolution =
                instance_cfg.as_ref().map(|c| c.seed_resolution.clone());
            let config = HybridSearchConfig {
                weight_ppr: weight_ppr.unwrap_or(defaults.weight_ppr),
                weight_bm25: weight_bm25.unwrap_or(defaults.weight_bm25),
                weight_semantic: weight_semantic.unwrap_or(defaults.weight_semantic),
                prf: prf_enabled,
                seed_resolution: configured_seed_resolution
                    .unwrap_or_else(|| defaults.seed_resolution.clone()),
                ..defaults
            };

            let aliases = load_alias_sidecar(&db_path);
            // Thread the parsed `--intent` override (if any) into the PPR
            // engine. None → engine auto-detects from seed kinds, matching
            // historical behavior.
            match build_brain_context_hybrid_with_aliases(
                &store,
                &seeds,
                tantivy.as_ref(),
                &config,
                &aliases,
                Some(&db_path),
                parsed_intent,
            ) {
                Ok(mut result) => {
                    // Feature F6: apply per-path ranking priors (dampen/boost)
                    // from `[ranking]` in the instance config, if supplied.
                    // Applied AFTER fusion on the final relevance, BEFORE the
                    // sort/truncation below. No config → no-op.
                    if let Some(ranking) = ranking_config.as_ref() {
                        nestweaver_engine::apply_ranking_priors(&mut result.seeds, ranking);
                        nestweaver_engine::apply_ranking_priors(&mut result.connected, ranking);
                        result.connected.sort_by(|a, b| {
                            b.relevance
                                .partial_cmp(&a.relevance)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    }

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

                    // `--no-tests`: drop rows whose location matches any
                    // configured seed-resolution path rule (prefix or
                    // suffix). Distinct from the soft deboost the ranking
                    // pass already applied — this removes the rows entirely
                    // so a strict-prod caller never sees them.
                    if no_tests {
                        let rules = &config.seed_resolution.path_deboost;
                        if !rules.is_empty() {
                            let is_test_path = |loc: &str| -> bool {
                                let lower = loc.to_lowercase();
                                rules.iter().any(|r| match (&r.prefix, &r.suffix) {
                                    (Some(prefix), None) => {
                                        let needle = prefix.trim_start_matches('/').to_lowercase();
                                        !needle.is_empty() && lower.contains(&needle)
                                    }
                                    (None, Some(suffix)) => loc.ends_with(suffix.as_str()),
                                    _ => false,
                                })
                            };
                            let drop_tests = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                                nodes.retain(|n| !is_test_path(&n.location));
                            };
                            drop_tests(&mut result.seeds);
                            drop_tests(&mut result.connected);
                        }
                    }

                    // `--prefer-instance <id>`: scope ranking to a single
                    // instance_id. UIDs encode the owning instance as a
                    // delimited segment: `note:vlt:<inst>:<hash>:...`,
                    // `sym:repo:<inst>:<hash>:...`, `repo:<inst>:<hash>`,
                    // etc. Matching on `:<inst>:` is robust to a partially-
                    // merged DB where some symbol UIDs still encode the
                    // pre-merge repo UID — both the old and new forms still
                    // carry an `:<inst>:` segment that uniquely identifies
                    // the instance, and the leading and trailing colons
                    // prevent accidental substring collisions with hashes.
                    if let Some(ref target) = prefer_instance {
                        let needle = format!(":{target}:");
                        let filter_inst = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|n| n.uid.contains(needle.as_str()));
                        };
                        filter_inst(&mut result.seeds);
                        filter_inst(&mut result.connected);
                    }

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

                    // Feature F17: rerank the top-N retrieved candidates. OFF by
                    // default → byte-identical output. Applied AFTER fusion +
                    // F6 priors + filters, BEFORE truncation. The default scorer
                    // is a transparent monotonic heuristic (NOT a validated nDCG
                    // win); an optional `<db>.rerank.json` learned-weights file
                    // is used if present and version-matched. Reranking only
                    // reorders an already-retrieved set; recall is unchanged.
                    if rerank {
                        let reranker = nestweaver_engine::select_reranker(Some(&db_path));
                        nestweaver_engine::rerank(
                            &mut result.connected,
                            reranker.as_ref(),
                            &store,
                            nestweaver_engine::RERANK_DEFAULT_TOP_N,
                        );
                    }

                    // Feature F8: embed high-relevance bodies inline when the
                    // caller opted in. Off by default → output unchanged.
                    if inline_bodies {
                        let root = root.clone().unwrap_or_else(|| {
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                        });
                        nestweaver_engine::populate_inline_bodies(
                            &store,
                            &mut result.connected,
                            &root,
                            response_config.inline_body_threshold,
                            response_config.inline_max_body_tokens,
                            token_budget,
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

        BrainCommands::BrokenLinks {
            max_suggestions,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            if let Some(value) = try_daemon_json_rpc(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_broken_links",
                serde_json::json!({ "max_suggestions": max_suggestions, "limit": limit }),
            ) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else if let Some(arr) = value.get("broken_links") {
                    let links: Vec<nestweaver_engine::BrokenLink> =
                        serde_json::from_value(arr.clone())?;
                    if links.is_empty() {
                        println!("No broken or ambiguous wikilinks found.");
                    } else {
                        println!("Broken / ambiguous wikilinks ({}):", links.len());
                        for l in &links {
                            println!(
                                "  [[{}]] in {} (confidence {:.2})",
                                l.wikilink_text, l.source_path, l.confidence
                            );
                            if !l.suggested_target_uids.is_empty() {
                                println!("    suggested: {}", l.suggested_target_uids.join(", "));
                            }
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let all_links = nestweaver_engine::broken_links(&store, max_suggestions)?;
            let total = all_links.len();
            let links: Vec<_> = all_links.into_iter().take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "broken_links": links,
                        "total": total,
                        "returned": links.len(),
                    }))?
                );
            } else if links.is_empty() {
                println!("No broken or ambiguous wikilinks found.");
            } else {
                println!("Broken / ambiguous wikilinks ({} of {total}):", links.len());
                for l in &links {
                    println!(
                        "  [[{}]] in {} (confidence {:.2})",
                        l.wikilink_text, l.source_path, l.confidence
                    );
                    if !l.suggested_target_uids.is_empty() {
                        println!("    suggested: {}", l.suggested_target_uids.join(", "));
                    }
                }
            }
            let stats = format!(
                "{} link(s) in {}",
                links.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::Orphans {
            vault,
            path_prefix,
            allow,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            {
                let mut args = serde_json::json!({});
                if let Some(ref v) = vault {
                    args["vault"] = serde_json::json!(v);
                }
                if let Some(ref p) = path_prefix {
                    args["path_prefix"] = serde_json::json!(p);
                }
                if !allow.is_empty() {
                    args["allowlist"] = serde_json::json!(allow);
                }
                args["limit"] = serde_json::json!(limit);
                if let Some(value) = try_daemon_json_rpc(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_orphan_documents",
                    args,
                ) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else if let Some(arr) = value.get("orphans") {
                        let orphans: Vec<nestweaver_engine::OrphanDocument> =
                            serde_json::from_value(arr.clone())?;
                        if orphans.is_empty() {
                            println!("No orphan documents found.");
                        } else {
                            println!("Orphan documents ({}):", orphans.len());
                            for o in &orphans {
                                println!("  {} — {}", o.title, o.file_path);
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let all_orphans = nestweaver_engine::orphan_documents(
                &store,
                vault.as_deref(),
                path_prefix.as_deref(),
                &allow,
            )?;
            let total = all_orphans.len();
            let orphans: Vec<_> = all_orphans.into_iter().take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "orphans": orphans,
                        "total": total,
                        "returned": orphans.len(),
                    }))?
                );
            } else if orphans.is_empty() {
                println!("No orphan documents found.");
            } else {
                println!("Orphan documents ({} of {total}):", orphans.len());
                for o in &orphans {
                    println!("  {} — {}", o.title, o.file_path);
                }
            }
            let stats = format!(
                "{} orphan(s) in {}",
                orphans.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::TopicClusters {
            resolution,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            if let Some(value) = try_daemon_json_rpc(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_topic_clusters",
                serde_json::json!({ "resolution": resolution, "limit": limit }),
            ) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else if let Some(arr) = value.get("clusters") {
                    let clusters: Vec<nestweaver_engine::TopicCluster> =
                        serde_json::from_value(arr.clone())?;
                    if clusters.is_empty() {
                        println!("No topic clusters found.");
                    } else {
                        println!("Topic clusters ({}):", clusters.len());
                        for c in &clusters {
                            println!(
                                "  [{}] {} ({} note(s))",
                                c.cluster_id,
                                c.label,
                                c.members.len()
                            );
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let all_clusters = nestweaver_engine::topic_clusters(&store, resolution)?;
            let total = all_clusters.len();
            let clusters: Vec<_> = all_clusters.into_iter().take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "clusters": clusters,
                        "total": total,
                        "returned": clusters.len(),
                    }))?
                );
            } else if clusters.is_empty() {
                println!("No topic clusters found.");
            } else {
                println!("Topic clusters ({} of {total}):", clusters.len());
                for c in &clusters {
                    println!(
                        "  [{}] {} ({} note(s))",
                        c.cluster_id,
                        c.label,
                        c.members.len()
                    );
                }
            }
            let stats = format!(
                "{} cluster(s) in {}",
                clusters.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::TagGraph {
            tag,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            {
                let mut args = serde_json::json!({ "limit": limit });
                if let Some(ref t) = tag {
                    args["tag"] = serde_json::json!(t);
                }
                if let Some(value) = try_daemon_json_rpc(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_tag_graph",
                    args,
                ) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else if tag.is_some() {
                        // Single-tag mode: value is a TagGraph directly.
                        let tg: nestweaver_engine::TagGraph = serde_json::from_value(value)?;
                        println!("#{} — {} note(s)", tg.tag, tg.count);
                        if tg.co_occurring.is_empty() {
                            println!("  no co-occurring tags");
                        } else {
                            println!("  co-occurring:");
                            for c in &tg.co_occurring {
                                println!("    #{} ({})", c.tag, c.count);
                            }
                        }
                    } else if let Some(arr) = value.get("tags") {
                        // All-tags mode.
                        let graphs: Vec<nestweaver_engine::TagGraph> =
                            serde_json::from_value(arr.clone())?;
                        if graphs.is_empty() {
                            println!("no tags");
                        } else {
                            for tg in &graphs {
                                let co = if tg.co_occurring.is_empty() {
                                    "—".to_string()
                                } else {
                                    tg.co_occurring
                                        .iter()
                                        .map(|c| format!("#{} ({})", c.tag, c.count))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                };
                                println!("#{} ({}) → {}", tg.tag, tg.count, co);
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            match tag {
                Some(tag) => {
                    let tg = nestweaver_engine::tag_graph(&store, &tag)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&tg)?);
                    } else {
                        println!("#{} — {} note(s)", tg.tag, tg.count);
                        if tg.co_occurring.is_empty() {
                            println!("  no co-occurring tags");
                        } else {
                            println!("  co-occurring:");
                            for c in &tg.co_occurring {
                                println!("    #{} ({})", c.tag, c.count);
                            }
                        }
                    }
                }
                None => {
                    let all_graphs = nestweaver_engine::tag_graph_all(&store)?;
                    let total = all_graphs.len();
                    let graphs: Vec<_> = all_graphs.into_iter().take(limit).collect();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "tags": graphs,
                                "total": total,
                                "returned": graphs.len(),
                            }))?
                        );
                    } else if graphs.is_empty() {
                        println!("no tags");
                    } else {
                        for tg in &graphs {
                            let co = if tg.co_occurring.is_empty() {
                                "—".to_string()
                            } else {
                                tg.co_occurring
                                    .iter()
                                    .map(|c| format!("#{} ({})", c.tag, c.count))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            };
                            println!("#{} ({}) → {}", tg.tag, tg.count, co);
                        }
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::DocStats {
            top_tags_limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;

            if let Some(value) = try_daemon_json_rpc(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_doc_stats",
                serde_json::json!({ "top_tags_limit": top_tags_limit }),
            ) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    let stats: nestweaver_engine::DocStats = serde_json::from_value(value)?;
                    println!("Document graph stats:");
                    println!("  total notes:      {}", stats.total_notes);
                    println!("  total wikilinks:  {}", stats.total_wikilinks);
                    println!("  broken wikilinks: {}", stats.broken_wikilinks);
                    println!("  orphans:          {}", stats.orphans);
                    println!("  avg out-degree:   {:.2}", stats.avg_outdegree);
                    if !stats.top_tags.is_empty() {
                        println!("  top tags:");
                        for t in &stats.top_tags {
                            println!("    #{} ({})", t.tag, t.count);
                        }
                    }
                    if !stats.notes_by_year.is_empty() {
                        let mut years: Vec<(&String, &usize)> =
                            stats.notes_by_year.iter().collect();
                        years.sort_by(|a, b| a.0.cmp(b.0));
                        println!("  notes by year:");
                        for (year, count) in years {
                            println!("    {year}: {count}");
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let stats = nestweaver_engine::doc_stats(&store, top_tags_limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("Document graph stats:");
                println!("  total notes:      {}", stats.total_notes);
                println!("  total wikilinks:  {}", stats.total_wikilinks);
                println!("  broken wikilinks: {}", stats.broken_wikilinks);
                println!("  orphans:          {}", stats.orphans);
                println!("  avg out-degree:   {:.2}", stats.avg_outdegree);
                if !stats.top_tags.is_empty() {
                    println!("  top tags:");
                    for t in &stats.top_tags {
                        println!("    #{} ({})", t.tag, t.count);
                    }
                }
                if !stats.notes_by_year.is_empty() {
                    let mut years: Vec<(&String, &usize)> = stats.notes_by_year.iter().collect();
                    years.sort_by(|a, b| a.0.cmp(b.0));
                    println!("  notes by year:");
                    for (year, count) in years {
                        println!("    {year}: {count}");
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
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

fn context_token_budgeted_truncate(
    connected: &[nestweaver_engine::ContextNode],
    budget: usize,
) -> usize {
    let mut tokens = 0usize;
    let mut taken = 0usize;
    for n in connected {
        let cost = (n.uid.len()
            + n.name.len()
            + n.kind.len()
            + n.file_path.len()
            + n.signature.len()
            + 20)
            .div_ceil(4);
        if tokens + cost > budget {
            break;
        }
        tokens += cost;
        taken += 1;
    }
    taken
}

/// A note-level search result after grouping BM25 hits by parent note.
struct NoteSearchResult {
    note_uid: String,
    title: String,
    best_score: f32,
    matched_headings: Vec<String>,
}

/// Group BM25 search hits by their parent Note, picking the highest-scoring
/// hit per note and collecting matched heading/section titles.
fn group_bm25_hits_by_note(
    store: &nestweaver_store::GraphStore,
    hits: &[nestweaver_store::SearchHit],
    limit: usize,
) -> Vec<NoteSearchResult> {
    use std::collections::HashMap;

    struct Group {
        note_uid: String,
        best_score: f32,
        title: String,
        matched_headings: Vec<String>,
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut note_order: Vec<String> = Vec::new();

    for h in hits {
        let parent_note_uid = match h.kind.as_str() {
            "note" => h.uid.clone(),
            "heading" => store
                .lookup_heading(&h.uid)
                .map(|hd| hd.note_uid)
                .unwrap_or_else(|_| h.uid.clone()),
            "section" => store
                .lookup_section(&h.uid)
                .map(|s| s.note_uid)
                .unwrap_or_else(|_| h.uid.clone()),
            _ => h.uid.clone(),
        };

        let group = groups.entry(parent_note_uid.clone()).or_insert_with(|| {
            note_order.push(parent_note_uid.clone());
            Group {
                note_uid: parent_note_uid.clone(),
                best_score: 0.0,
                title: String::new(),
                matched_headings: Vec::new(),
            }
        });

        if h.score > group.best_score {
            group.best_score = h.score;
        }
        if h.kind == "note" {
            group.title = h.title.clone();
        }
        if h.kind == "heading" || h.kind == "section" {
            group.matched_headings.push(h.title.clone());
        }
    }

    // Look up note titles for groups that had no direct note-title match.
    for group in groups.values_mut() {
        if group.title.is_empty() {
            group.title = store
                .lookup_note(&group.note_uid)
                .map(|n| n.title)
                .unwrap_or_else(|_| group.note_uid.clone());
        }
    }

    // Sort by best_score descending.
    note_order.sort_by(|a, b| {
        let sa = groups.get(a).map(|g| g.best_score).unwrap_or(0.0);
        let sb = groups.get(b).map(|g| g.best_score).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    note_order
        .into_iter()
        .take(limit)
        .filter_map(|nuid| {
            groups.remove(&nuid).map(|g| NoteSearchResult {
                note_uid: g.note_uid,
                title: g.title,
                best_score: g.best_score,
                matched_headings: g.matched_headings,
            })
        })
        .collect()
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
/// Aligned with the MCP render_cost to avoid CLI vs MCP divergence.
fn render_cost_tokens(n: &nestweaver_engine::BrainNode) -> usize {
    // UID + title + kind + location + relevance (~10 chars) + JSON overhead
    (n.uid.len() + n.title.len() + n.kind.len() + n.location.len() + 10 + 80).div_ceil(4)
}

fn print_brain_context_text(result: &BrainContextResult, cut: usize, token_budget: Option<usize>) {
    // Feature F7: show PRF-mined expansion terms for auditing.
    if !result.expansion_terms.is_empty() {
        println!("PRF expansion terms: {}", result.expansion_terms.join(", "));
        println!();
    }
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
            if let Some(body) = &n.inline_body {
                for line in body.lines() {
                    println!("      | {line}");
                }
            }
        }
    }
}

/// Render a daemon-routed `BrainSearchResponse` in the same shape as the
/// direct-disk `brain search` handler. The daemon merges note + symbol
/// results into a single `results` array, distinguished by `kind`
/// (`"note"` vs `"Symbol/<Kind>"`); text mode splits them back out so the
/// per-row format matches the legacy output.
fn render_brain_search_response(
    resp: &nestweaver_proto::BrainSearchResponse,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(resp.results.len());
        for item in &resp.results {
            let mut v = serde_json::json!({
                "uid": item.uid,
                "kind": item.kind,
                "title": item.title,
                "score": item.score,
            });
            if let Some(ref loc) = item.location {
                v["location"] = serde_json::json!(loc);
            }
            if !item.matched_headings.is_empty() {
                v["matched_headings"] = serde_json::json!(item.matched_headings);
            }
            if let Some(ref body) = item.inline_body {
                v["inline_body"] = serde_json::json!(body);
            }
            results.push(v);
        }
        let mut payload = serde_json::json!({
            "query": resp.query,
            "engine": resp.engine,
            "results": results,
            "total_matches": resp.total_matches,
        });
        if !resp.expansion_terms.is_empty() {
            payload["expansion_terms"] = serde_json::json!(resp.expansion_terms);
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if resp.results.is_empty() {
        println!("No results for '{}'.", resp.query);
        return Ok(());
    }

    let header = if resp.engine == "bm25" {
        "Brain search (BM25)"
    } else {
        "Brain search (substring fallback)"
    };
    println!("{}: {} result(s)", header, resp.results.len());
    if !resp.expansion_terms.is_empty() {
        println!("  PRF expansion terms: {}", resp.expansion_terms.join(", "));
    }
    println!();
    for item in &resp.results {
        if item.kind == "note" {
            if item.matched_headings.is_empty() {
                println!("  [{:.2}] {}", item.score, item.title);
            } else {
                println!(
                    "  [{:.2}] {} (matched: {})",
                    item.score,
                    item.title,
                    item.matched_headings.join(", "),
                );
            }
        } else {
            // Symbol/<Kind> row: split "kind" prefix off, render with location.
            let kind_short = item.kind.strip_prefix("Symbol/").unwrap_or(&item.kind);
            if let Some(ref loc) = item.location {
                println!(
                    "  [{:.2}] {} [{}] @ {}",
                    item.score, item.title, kind_short, loc,
                );
            } else {
                println!("  [{:.2}] {} [{}]", item.score, item.title, kind_short);
            }
        }
    }
    Ok(())
}

/// Render the daemon's `project_context` JSON response (shape produced by
/// `tool_project_context` in nestweaver-mcp). When `json` is true, emit the
/// response verbatim; otherwise print a project header followed by the
/// connected nodes.
fn render_project_context_daemon_response(
    value: &serde_json::Value,
    json: bool,
    token_budget: usize,
) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("warning: failed to serialize daemon response: {e}"),
        }
        return;
    }
    let project = value.get("project").and_then(|v| v.as_str()).unwrap_or("");
    let project_uid = value
        .get("project_uid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let used = value
        .get("tokens_used")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("Project: {project}  ({project_uid})");
    if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
        println!("  {note}");
    }
    println!();
    let empty = vec![];
    let connected = value
        .get("connected")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    println!("Connected ({} item(s)):", connected.len());
    for n in connected {
        let title = n.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let location = n.get("location").and_then(|v| v.as_str()).unwrap_or("");
        let rel = n.get("relevance").and_then(|v| v.as_f64()).unwrap_or(0.0);
        println!("  [{rel:.4}] {kind}  {title}  @{location}");
    }
    println!();
    println!("Tokens used: {used} / budget: {token_budget}");
}

fn print_brain_context_json(result: &BrainContextResult, limit: usize) -> anyhow::Result<()> {
    let mut resp = serde_json::json!({
        "seeds_expanded": result.seeds.len(),
        "connected": result.connected.iter().take(limit).collect::<Vec<_>>(),
    });

    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = serde_json::json!(result.unresolved_seeds);
    }

    // Feature F7: surface PRF-mined expansion terms for auditing.
    if !result.expansion_terms.is_empty() {
        resp["expansion_terms"] = serde_json::json!(result.expansion_terms);
    }

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

/// Render the `project-context` JSON for the local (--no-daemon) path with
/// the same wrapper shape the daemon / MCP `project_context` tool emits:
/// `project`, `project_uid`, `seeds_expanded`, `connected`, `tokens_used`,
/// `token_budget`, and optional `unresolved_seeds` / `external_refs`. Agents
/// depend on these fields, so the local and daemon paths must stay aligned.
fn print_project_context_json(
    project: &nestweaver_schema::Project,
    result: &BrainContextResult,
    limit: usize,
    tokens_used: usize,
    token_budget: usize,
    external_refs: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut resp = serde_json::json!({
        "project": project.name,
        "project_uid": project.uid,
        "seeds_expanded": result.seeds.len(),
        "connected": result.connected.iter().take(limit).collect::<Vec<_>>(),
        "tokens_used": tokens_used,
        "token_budget": token_budget,
    });
    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = serde_json::json!(result.unresolved_seeds);
    }
    if !result.expansion_terms.is_empty() {
        resp["expansion_terms"] = serde_json::json!(result.expansion_terms);
    }
    if !external_refs.is_null() {
        resp["external_refs"] = external_refs.clone();
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

/// Resolve a `--repo` filter (display name or literal UID) to a repo UID.
/// Returns `Ok(None)` when no filter was given, or an error when the filter
/// matches no indexed repo.
fn resolve_contract_repo_filter(
    store: &GraphStore,
    filter: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let repos = list_repos(store, None)?;
    // Exact UID match first, then display-name (case-insensitive) match.
    if let Some(r) = repos.iter().find(|r| r.uid == filter) {
        return Ok(Some(r.uid.clone()));
    }
    let needle = filter.to_lowercase();
    if let Some(r) = repos
        .iter()
        .find(|r| nestweaver_engine::repo_display_name(r).to_lowercase() == needle)
    {
        return Ok(Some(r.uid.clone()));
    }
    anyhow::bail!("no indexed repo matches --repo '{filter}'")
}

fn run_contracts(command: ContractCommands) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        ContractCommands::List { repo, json, db } => {
            let store = open_store(db.as_deref())?;
            let repo_uid = resolve_contract_repo_filter(&store, repo.as_deref())?;
            let mut contracts = store
                .list_contracts(repo_uid.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
            contracts.sort_by(|a, b| a.uid.cmp(&b.uid));

            if json {
                println!("{}", serde_json::to_string_pretty(&contracts)?);
            } else if contracts.is_empty() {
                println!(
                    "No contracts found. Index a repo with OpenAPI/proto/GraphQL specs or \
                     Spring/NestJS controllers first."
                );
            } else {
                println!(
                    "API contracts ({} total). NOTE: contract links are hypotheses, \
                     not ground truth — see confidence.\n",
                    contracts.len()
                );
                for c in &contracts {
                    println!("{}", c.uid);
                    println!("  kind:       {}", c.kind);
                    if let Some(ref v) = c.verb {
                        println!("  verb:       {v}");
                    }
                    if let Some(ref p) = c.path {
                        println!("  path:       {p}");
                    }
                    if let Some(ref op) = c.operation_id {
                        println!("  operation:  {op}");
                    }
                    println!("  source:     {}", c.source_path);
                    println!("  confidence: {:.2}", c.confidence);
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }
        ContractCommands::Drift { repo, json, db } => {
            let store = open_store(db.as_deref())?;
            let repo_uid = resolve_contract_repo_filter(&store, repo.as_deref())?;
            let report = nestweaver_engine::contracts::drift_for_store(&store, repo_uid.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.is_clean() {
                println!("No contract drift detected.");
            } else {
                println!("Contract drift (hypotheses, not ground truth):\n");
                if !report.declared_not_implemented.is_empty() {
                    println!(
                        "Declared but NOT implemented ({}):",
                        report.declared_not_implemented.len()
                    );
                    for f in &report.declared_not_implemented {
                        println!("  - {}", f.uid);
                    }
                    println!();
                }
                if !report.implemented_not_declared.is_empty() {
                    println!(
                        "Implemented but NOT declared in any spec ({}):",
                        report.implemented_not_declared.len()
                    );
                    for f in &report.implemented_not_declared {
                        println!("  - {}", f.uid);
                    }
                    println!();
                }
            }
            Ok((EXIT_SUCCESS, None))
        }
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
        InstanceCommands::Remove {
            id,
            purge_graph,
            db,
        } => {
            let mut registry =
                nestweaver_engine::Registry::load_or_create(&default_registry_path())?;
            let registry_removed = match registry.remove(&id) {
                Ok(()) => true,
                Err(e) => {
                    // With --purge-graph we tolerate a missing registry
                    // entry so ghost instances (left by a misconfigured
                    // merge) can still be cleaned out of the graph.
                    if purge_graph {
                        eprintln!("Note: {e}; continuing with graph purge");
                        false
                    } else {
                        return Err(e);
                    }
                }
            };
            if registry_removed {
                println!("Removed instance '{id}' from registry");
            }
            if purge_graph {
                let db_path = db.unwrap_or_else(default_db_path);
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt
                    .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                    .context("failed to connect to daemon")?;

                let mut stream = rt
                    .block_on(client.purge_instance(&id))
                    .context("purge_instance RPC failed")?;

                let mut had_error = false;
                rt.block_on(async {
                    while let Ok(Some(p)) = stream.message().await {
                        eprintln!("{}", p.message);
                        if p.phase == nestweaver_proto::Phase::Error as i32 {
                            had_error = true;
                        }
                    }
                });

                if had_error {
                    return Err(anyhow::anyhow!("purge_instance failed"));
                }
            }
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
            nestweaver_engine::verify_snapshot(&dest).map_err(|e| {
                anyhow::anyhow!(
                    "pulled snapshot failed integrity check: {e}; \
                     the snapshot in storage may be corrupted"
                )
            })?;
            println!("Pulled snapshot v{} for '{}'", meta.version, id);
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Merge { from, to, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            let result = rt
                .block_on(client.merge_instance(&from, &to))
                .context("merge_instance RPC failed")?;

            if result.vaults_reparented + result.repos_reparented + result.projects_reparented == 0
            {
                println!("No rows found with instance_id '{from}'.");
            } else {
                println!(
                    "Merged '{from}' -> '{to}': {} vault(s), {} repo(s), {} project(s)",
                    result.vaults_reparented, result.repos_reparented, result.projects_reparented
                );
                for d in &result.discarded_vaults {
                    eprintln!("Note: {d}");
                }
            }
            Ok(EXIT_SUCCESS)
        }
    }
}

fn run_snapshot(command: SnapshotCommands) -> anyhow::Result<i32> {
    match command {
        SnapshotCommands::Build {
            instance,
            db,
            config,
            output,
        } => {
            // Resolve DB path: --db > --config > env/default
            let db_path = resolve_db_with_config(db, config.as_deref())?;

            if !db_path.exists() {
                anyhow::bail!(
                    "database not found at {}; run 'nestweaver index' first",
                    db_path.display()
                );
            }

            // Open the store to query repos
            let store = GraphStore::open_read_only(&db_path)
                .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;

            // Load instance config if provided
            let cfg = load_instance_config_opt(config.as_deref());

            // Resolve instance ID
            let instance_id = instance
                .or_else(|| cfg.as_ref().map(|c| c.instance_id.clone()))
                .unwrap_or_else(|| "standalone".to_string());

            // Schema hashes
            let core_hash = nestweaver_schema::core_schema_hash();
            let ext_hash = match cfg.as_ref().and_then(|c| c.schema_extensions.as_ref()) {
                Some(ext) => {
                    // Build a deterministic string from the extensions
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(ref props) = ext.extra_node_properties {
                        let mut labels: Vec<&String> = props.keys().collect();
                        labels.sort();
                        for label in labels {
                            let inner = &props[label];
                            let mut keys: Vec<&String> = inner.keys().collect();
                            keys.sort();
                            for key in keys {
                                parts.push(format!("{label}.{key}={}", inner[key]));
                            }
                        }
                    }
                    let joined = parts.join("\n");
                    use sha2::Digest;
                    let hash = sha2::Sha256::digest(joined.as_bytes());
                    hex::encode(hash)
                }
                None => "none".to_string(),
            };
            let effective_hash = nestweaver_schema::effective_schema_hash(&core_hash, &ext_hash);

            // Embedding info
            let embedding_model_id = cfg
                .as_ref()
                .map(|c| c.inference.embedding_model.clone())
                .unwrap_or_else(|| "unknown".to_string());

            // Timestamp (RFC3339 UTC)
            let built_at = {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs();
                // Format as RFC3339 UTC
                let days = secs / 86400;
                let time_secs = secs % 86400;
                let hours = time_secs / 3600;
                let minutes = (time_secs % 3600) / 60;
                let seconds = time_secs % 60;

                // Convert days since epoch to y/m/d
                // Algorithm from http://howardhinnant.github.io/date_algorithms.html
                let z = days as i64 + 719468;
                let era = if z >= 0 { z } else { z - 146096 } / 146097;
                let doe = (z - era * 146097) as u64;
                let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
                let y = yoe as i64 + era * 400;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let d = doy - (153 * mp + 2) / 5 + 1;
                let m = if mp < 10 { mp + 3 } else { mp - 9 };
                let y = if m <= 2 { y + 1 } else { y };
                format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
            };

            // Repos
            let repos = nestweaver_engine::list_repos(&store, Some(&instance_id))?;

            let repo_stamps: Vec<nestweaver_engine::RepoStamp> = repos
                .iter()
                .map(|r| nestweaver_engine::RepoStamp {
                    url: r.url.clone(),
                    indexed_sha: r.indexed_sha.clone(),
                    commits_behind_head: r.staleness_commits_behind,
                })
                .collect();

            let stamp = nestweaver_engine::Stamp {
                instance_id: instance_id.clone(),
                engine_version: env!("CARGO_PKG_VERSION").to_string(),
                min_compatible_engine: nestweaver_engine::MIN_SNAPSHOT_READER_VERSION.to_string(),
                schema_hash_core: core_hash,
                schema_hash_extensions: ext_hash,
                schema_hash_effective: effective_hash,
                embedding_model_id,
                embedding_dimension: store.embedding_dimension().unwrap_or(0),
                built_at,
                repos: repo_stamps,
            };

            let manifest = nestweaver_engine::Manifest {
                repos: repos
                    .iter()
                    .map(|r| nestweaver_engine::ManifestRepo {
                        url: r.url.clone(),
                        indexed_sha: r.indexed_sha.clone(),
                        files_skipped: Vec::new(),
                    })
                    .collect(),
            };

            let output_dir = output.unwrap_or_else(|| {
                db_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(format!("snapshot-{instance_id}"))
            });

            nestweaver_engine::build_snapshot(&output_dir, &stamp, &manifest, &db_path)?;

            println!("Snapshot built successfully in {}", output_dir.display());
            println!("  Instance: {}", stamp.instance_id);
            println!("  Engine: {}", stamp.engine_version);
            println!("  Schema: {}", stamp.schema_hash_effective);
            println!("  Repos: {}", stamp.repos.len());
            Ok(EXIT_SUCCESS)
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
        SnapshotCommands::Push {
            instance,
            config,
            snapshot_dir,
            backend,
            backend_path,
        } => {
            // Resolve snapshot directory and backend from args/config/instance registry.
            let (snap_dir, backend_name, b_path) = if let Some(inst_id) = instance {
                // Load from registry → instance config
                let registry =
                    nestweaver_engine::Registry::load_or_create(&default_registry_path())?;
                let entry = registry
                    .get(&inst_id)
                    .ok_or_else(|| anyhow::anyhow!("instance '{}' not registered", inst_id))?;
                let cfg =
                    nestweaver_engine::InstanceConfig::from_file(Path::new(&entry.config_path))?;
                let dir = snapshot_dir.unwrap_or_else(|| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("nestweaver")
                        .join(&inst_id)
                        .join("snapshot")
                });
                let be_name = backend.unwrap_or(cfg.snapshot_storage.backend);
                let be_path = backend_path.or(cfg.snapshot_storage.path);
                (dir, be_name, be_path)
            } else if let Some(ref cfg_path) = config {
                // Load from explicit config file
                let cfg = nestweaver_engine::InstanceConfig::from_file(cfg_path)?;
                let dir = snapshot_dir.unwrap_or_else(|| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("nestweaver")
                        .join(&cfg.instance_id)
                        .join("snapshot")
                });
                let be_name = backend.unwrap_or(cfg.snapshot_storage.backend);
                let be_path = backend_path.or(cfg.snapshot_storage.path);
                (dir, be_name, be_path)
            } else if let (Some(b), Some(dir)) = (backend, snapshot_dir) {
                // Direct flags: --backend + --snapshot-dir
                (dir, b, backend_path)
            } else {
                anyhow::bail!("provide --instance, --config, or both --backend and --snapshot-dir");
            };

            // Verify integrity first
            let stamp = nestweaver_engine::verify_snapshot(&snap_dir)
                .map_err(|e| anyhow::anyhow!("snapshot integrity check failed: {e}"))?;

            // Build meta from stamp
            let meta = nestweaver_storage::SnapshotMeta {
                version: stamp.engine_version.clone(),
                instance_id: stamp.instance_id.clone(),
            };

            // Create backend and push
            let storage = nestweaver_storage::create_backend(&backend_name, b_path.as_deref())?;
            storage.push_snapshot(&snap_dir, &meta)?;

            println!(
                "Snapshot pushed: instance='{}' version='{}'",
                meta.instance_id, meta.version
            );
            Ok(EXIT_SUCCESS)
        }
    }
}

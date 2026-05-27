mod setup;

use std::path::{Path, PathBuf};
use std::process;

use anyhow::Context;
use clap::{Parser, Subcommand};
use nestweaver_engine::{
    BrainContextResult, BrainWatcher, ContextResult, FeatureContextResult, HybridSearchConfig,
    LookupResult, build_brain_context_hybrid_with_aliases, build_context, build_feature_context,
    compute_clusters, discover_cross_domain_links, embedding::generate_embedding,
    generate_agents_md, generate_cursor_rule, generate_guide, generate_repo_map, generate_skill,
    incremental_index, index_directory, index_markdown_directory, index_markdown_directory_since,
    list_repos, list_services, load_alias_sidecar, load_clusters, load_manifest_cache,
    lookup_symbol, save_clusters, search_symbols, suggest_links,
};
use nestweaver_schema::Symbol;
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};

// ── Exit codes ────────────────────────────────────────────────────────────────
const EXIT_SUCCESS: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_NOT_FOUND: i32 = 2;
const EXIT_AMBIGUOUS: i32 = 3;

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
                  Default database: ./nestweaver.lbug"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    /// Detects Claude Code, Cursor, Codex, Windsurf, JetBrains, and VS Code,
    /// then writes the correct MCP server config and instruction files for each.
    #[command(
        after_help = "Examples:\n  nestweaver setup\n  nestweaver setup --all\n  nestweaver setup claude-code"
    )]
    Setup {
        /// Configure a specific tool (claude-code, cursor, codex, windsurf, jetbrains, vscode)
        tool: Option<String>,
        /// Force-configure all tools even if not detected
        #[arg(long)]
        all: bool,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let exit_code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            EXIT_ERROR
        }
    };

    process::exit(exit_code);
}

fn run(cli: Cli) -> anyhow::Result<i32> {
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
            Ok(EXIT_SUCCESS)
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
            Ok(EXIT_SUCCESS)
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
                    Ok(EXIT_SUCCESS)
                }
                None => {
                    eprintln!("Service not found: {name}");
                    Ok(EXIT_NOT_FOUND)
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
            Ok(EXIT_SUCCESS)
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
                    Ok(EXIT_SUCCESS)
                }
                ResolveResult::NotFound => {
                    eprintln!("Symbol '{name_or_uid}' not found.");
                    Ok(EXIT_NOT_FOUND)
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
                    Ok(EXIT_AMBIGUOUS)
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
                    Ok(EXIT_SUCCESS)
                }
                Err(e) => {
                    eprintln!("Pull failed: {e}");
                    Ok(e.exit_code())
                }
            }
        }

        Commands::Context {
            seeds,
            feature,
            config,
            json,
            db,
        } => {
            let store = open_store(db.as_deref())?;

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
                        if json {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        } else {
                            print_feature_context_text(&result);
                        }
                        Ok(EXIT_SUCCESS)
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("No symbols found") {
                            eprintln!("{msg}");
                            Ok(EXIT_NOT_FOUND)
                        } else {
                            eprintln!("Error: {msg}");
                            Ok(EXIT_ERROR)
                        }
                    }
                }
            } else {
                // Normal seed-based context.
                match build_context(&store, &seeds) {
                    Ok(result) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        } else {
                            print_context_text(&result);
                        }
                        Ok(EXIT_SUCCESS)
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("No matching symbols") {
                            eprintln!("{msg}");
                            Ok(EXIT_NOT_FOUND)
                        } else if msg.contains("Ambiguous") {
                            eprintln!("{msg}");
                            Ok(EXIT_AMBIGUOUS)
                        } else {
                            eprintln!("Error: {msg}");
                            Ok(EXIT_ERROR)
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
            Ok(EXIT_SUCCESS)
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
            Ok(EXIT_SUCCESS)
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
                    return Ok(EXIT_SUCCESS);
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
            Ok(EXIT_SUCCESS)
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
                    eprintln!("Guide written to {}", path.display());
                }
                None => print!("{output_str}"),
            }
            Ok(EXIT_SUCCESS)
        }

        Commands::Clusters {
            resolution,
            json,
            db,
        } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let store = open_store(Some(&db_path))?;

            eprintln!("Computing clusters (resolution={resolution})...");
            let output = compute_clusters(&store, resolution)?;
            save_clusters(&db_path, &output)?;
            eprintln!(
                "Found {} community(ies), modularity={:.4}. Saved to sidecar.",
                output.communities.len(),
                output.modularity
            );

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
            Ok(EXIT_SUCCESS)
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
                    eprintln!("No cached clusters found; computing with default resolution...");
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
                    Ok(EXIT_SUCCESS)
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
                    Ok(EXIT_NOT_FOUND)
                }
            }
        }

        Commands::Setup { tool, all, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            setup::run_setup(tool.as_deref(), &db_path, all)?;
            Ok(EXIT_SUCCESS)
        }

        Commands::Snapshot { command } => run_snapshot(command),
        Commands::Instance { command } => run_instance(command),
        Commands::Brain { command } => run_brain(*command),
        Commands::Embed {
            db,
            endpoint,
            model,
            batch_size,
        } => run_embed(db.as_deref(), &endpoint, &model, batch_size),

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
            Ok(EXIT_SUCCESS)
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
            let tantivy = TantivyIndex::open_or_create(&tantivy_path).ok();

            let state = nestweaver_web::state::AppState::new(store, tantivy, db_path);

            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(nestweaver_web::start_server(state, port, !no_open))?;

            Ok(EXIT_SUCCESS)
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
            Ok(EXIT_SUCCESS)
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
                        println!("Symbol: {}", s.name);
                        println!("Kind: {}", s.kind);
                        println!("File: {}:{}", s.file_path, s.start_line);
                        println!("Signature: {}", s.signature);

                        if !detail.callers.is_empty() {
                            println!("\nCallers ({}):", detail.callers.len());
                            for c in &detail.callers {
                                println!("  {} ({}:{})", c.name, c.file_path, c.start_line);
                            }
                        }

                        if !detail.callees.is_empty() {
                            println!("\nCallees ({}):", detail.callees.len());
                            for c in &detail.callees {
                                println!("  {} ({}:{})", c.name, c.file_path, c.start_line);
                            }
                        }
                    }
                    Ok(EXIT_SUCCESS)
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
                    Ok(EXIT_NOT_FOUND)
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
                    Ok(EXIT_AMBIGUOUS)
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
                        println!("No impact found for '{name_or_uid}'.");
                    } else {
                        println!("Impact of '{name_or_uid}' ({} nodes):", nodes.len());
                        for n in &nodes {
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
                    Ok(EXIT_SUCCESS)
                }
                ResolveResult::NotFound => {
                    eprintln!("Symbol '{name_or_uid}' not found.");
                    Ok(EXIT_NOT_FOUND)
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
                    Ok(EXIT_AMBIGUOUS)
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
            Ok(EXIT_SUCCESS)
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
            let tantivy = TantivyIndex::open_or_create(&tantivy_path).ok();

            // Resolve the project.
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
                        eprintln!(
                            "Project '{}' not found. Try: nestweaver list-projects",
                            name
                        );
                        return Ok(EXIT_NOT_FOUND);
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
                return Ok(EXIT_SUCCESS);
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
            ) {
                Ok(mut result) => {
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
                    Ok(EXIT_SUCCESS)
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    Ok(EXIT_ERROR)
                }
            }
        }

        Commands::Index {
            repo,
            instance,
            db,
            force,
        } => {
            let repo_path = repo.clone().unwrap_or_else(|| PathBuf::from("."));
            let db_path = db.unwrap_or_else(default_db_path);
            let instance_id = instance.as_deref().unwrap_or("default");

            let repo_url = format!("file://{}", repo_path.display());

            eprintln!("Indexing {} → {}", repo_path.display(), db_path.display());

            if force {
                // Full re-index requested explicitly.
                let result = index_directory(&repo_path, &db_path, instance_id, &repo_url, "local")
                    .context("index_directory")?;

                println!(
                    "Indexed {} file(s), {} symbol(s), {} edge(s).",
                    result.files_count, result.symbols_count, result.edges_count
                );

                if !result.skipped_files.is_empty() {
                    eprintln!("Skipped {} file(s):", result.skipped_files.len());
                    for sf in &result.skipped_files {
                        eprintln!("  {} — {}", sf.path, sf.reason);
                    }
                }
            } else {
                // Incremental index (falls back to full when no prior index exists).
                let inc = incremental_index(&repo_path, &db_path, instance_id, &repo_url)
                    .context("incremental_index")?;

                if inc.fell_back_to_full {
                    eprintln!("Incremental: no prior index found, performed full index.");
                } else {
                    eprintln!(
                        "Incremental: {} added, {} modified, {} deleted, {} renamed, {} skipped.",
                        inc.files_added,
                        inc.files_modified,
                        inc.files_deleted,
                        inc.files_renamed,
                        inc.files_skipped,
                    );
                    eprintln!(
                        "Incremental: {} symbol(s) added, {} symbol(s) removed.",
                        inc.symbols_added, inc.symbols_removed,
                    );
                }
            }

            // Compute PageRank after indexing so the repo-map is immediately usable.
            // (incremental_index already computes PageRank internally, but recomputing
            // here is cheap and ensures the sidecar is always up to date for the full path.)
            eprintln!("Computing PageRank...");
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
            eprintln!("PageRank complete.");

            Ok(EXIT_SUCCESS)
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

/// Best-effort Ctrl-C handler. We don't pull in a signal-handling crate
/// (`ctrlc`, `signal-hook`) for v1 — process-kill is good enough since
/// our writes are transactional. This function is a stub so callers can
/// optionally upgrade later without changing call sites.
fn ctrlc_handler<F>(_on_signal: F) -> Result<(), ()>
where
    F: FnMut() + Send + 'static,
{
    // Intentionally a no-op. SIGINT terminates the process; lbug's WAL
    // ensures the on-disk graph stays consistent.
    Ok(())
}

/// Dispatch a `brain` subcommand.
fn run_brain(command: BrainCommands) -> anyhow::Result<i32> {
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
                return Ok(EXIT_ERROR);
            }
            if !path.is_dir() {
                eprintln!("Error: path is not a directory: {}", path.display());
                return Ok(EXIT_ERROR);
            }

            // Auto-detect: report what we think the source is.
            let has_obsidian = path.join(".obsidian").is_dir();
            let kind_hint = if has_obsidian {
                "Obsidian vault"
            } else {
                "markdown folder"
            };
            eprintln!(
                "Detected {} at {} -> {}",
                kind_hint,
                path.display(),
                db_path.display()
            );

            let result = index_markdown_directory(&path, &db_path, instance_id, &vault_name)
                .context("index_markdown_directory")?;

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
                eprintln!("Skipped {} file(s):", result.skipped.len());
                for sf in &result.skipped {
                    eprintln!("  {} - {}", sf.path, sf.reason);
                }
            }

            Ok(EXIT_SUCCESS)
        }

        BrainCommands::List { json, db } => {
            let store = open_store(db.as_deref())?;
            let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;

            // Compute (note_count, last_indexed) per vault once. last_indexed
            // is max(modified_at) across the vault's notes — captures the most
            // recent file mtime the indexer/watcher saw.
            let per_vault: Vec<(String, usize, Option<String>)> = vaults
                .iter()
                .map(|v| {
                    let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
                    let last = notes.iter().filter_map(|n| n.modified_at.clone()).max();
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
            Ok(EXIT_SUCCESS)
        }

        BrainCommands::Status { json, db } => {
            let store = open_store(db.as_deref())?;
            let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;
            let note_count = store.count_notes().map_err(|e| anyhow::anyhow!(e))?;
            let heading_count = store.count_headings().map_err(|e| anyhow::anyhow!(e))?;
            let section_count = store.count_sections().map_err(|e| anyhow::anyhow!(e))?;
            let tag_count = store.count_tags().map_err(|e| anyhow::anyhow!(e))?;
            let wikilink_count = store
                .count_wikilink_edges()
                .map_err(|e| anyhow::anyhow!(e))?;
            let repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;

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
                        let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
                        let vault_note_count = notes.len();
                        let last_indexed = notes
                            .iter()
                            .filter_map(|n| n.modified_at.as_deref())
                            .max()
                            .map(|s| s.to_string());
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
                let db_default = default_db_path();
                let db_path = db.as_deref().unwrap_or(&db_default).display().to_string();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Status {
                        db: db_path,
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
                let db_default = default_db_path();
                println!(
                    "  Database:  {}",
                    db.as_deref().unwrap_or(&db_default).display()
                );
                println!("  Vaults:    {}", vaults.len());
                for v in &vaults {
                    let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
                    let vault_note_count = notes.len();
                    let last_indexed = notes
                        .iter()
                        .filter_map(|n| n.modified_at.as_deref())
                        .max()
                        .map(|s| s.to_string())
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
            Ok(EXIT_SUCCESS)
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
                return Ok(EXIT_ERROR);
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

            eprintln!(
                "Watching {} -> {} (Ctrl-C to stop)",
                path.display(),
                db_path.display()
            );
            watcher.run().context("watcher")?;

            // Clean up the lock file on orderly shutdown.
            let _ = std::fs::remove_file(&lock_path);
            eprintln!("Watcher stopped.");
            Ok(EXIT_SUCCESS)
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
                return Ok(EXIT_ERROR);
            }
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
            let instance_id = instance.unwrap_or_else(|| "default".to_string());

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
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let v_uid =
                    nestweaver_schema::vault_uid(&instance_id, &canonical.to_string_lossy());

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
            Ok(EXIT_SUCCESS)
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
            Ok(EXIT_SUCCESS)
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
            Ok(EXIT_SUCCESS)
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
            let tantivy = TantivyIndex::open_or_create(&tantivy_path).ok();

            if let Some(ref idx) = tantivy {
                let hits = idx
                    .search(&query, limit)
                    .with_context(|| "tantivy search")?;
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
            Ok(EXIT_SUCCESS)
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
            let tantivy = TantivyIndex::open_or_create(&tantivy_path).ok();

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
                    if json {
                        print_brain_context_json(&result, cut)?;
                    } else {
                        print_brain_context_text(&result, cut, token_budget);
                    }
                    Ok(EXIT_SUCCESS)
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("No seeds resolved") {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "seeds": [],
                                    "connected": [],
                                    "unresolved_seeds": seeds,
                                }))?
                            );
                        } else {
                            eprintln!("{msg}");
                        }
                        Ok(EXIT_NOT_FOUND)
                    } else {
                        eprintln!("Error: {msg}");
                        Ok(EXIT_ERROR)
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

/// Parse an ISO 8601 string to Unix epoch seconds (f64).
/// Delegates to the shared implementation in nestweaver-engine.
fn cli_parse_iso8601_to_epoch(s: &str) -> f64 {
    nestweaver_engine::parse_iso8601_to_epoch(s)
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
    let note_timestamps: std::collections::HashMap<String, f64> =
        store.list_notes(None).unwrap_or_default().into_iter()
            .filter_map(|n| n.modified_at.map(|t| (n.uid, cli_parse_iso8601_to_epoch(&t))))
            .collect();
    let section_note_map: std::collections::HashMap<String, String> =
        store.list_all_sections().unwrap_or_default().into_iter()
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
    #[derive(serde::Serialize)]
    struct Truncated<'a> {
        seeds: &'a [nestweaver_engine::BrainNode],
        connected: Vec<&'a nestweaver_engine::BrainNode>,
        unresolved_seeds: &'a [String],
    }
    let truncated = Truncated {
        seeds: &result.seeds,
        connected: result.connected.iter().take(limit).collect(),
        unresolved_seeds: &result.unresolved_seeds,
    };
    println!("{}", serde_json::to_string_pretty(&truncated)?);
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

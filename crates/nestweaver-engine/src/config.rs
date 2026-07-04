use nestweaver_store::{PathDeboostRule, SEED_PATH_FACTOR_MAX, SEED_PATH_FACTOR_MIN};
// Re-export seed-resolution primitives so callers can construct/observe them
// without depending on `nestweaver-store` directly.
pub use nestweaver_store::{SeedResolutionConfig, default_kind_priority};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bounds applied to a ranking-prior multiplier and to the final product
/// after a prior is applied. A multiplier below the floor would erase a
/// result; above the ceiling would let a prior dominate every other signal.
pub const RANKING_MULTIPLIER_MIN: f64 = 0.05;
pub const RANKING_MULTIPLIER_MAX: f64 = 5.0;

/// A single path-glob ranking rule (Feature F6). `glob` is matched against a
/// result's file-path location; `multiplier` scales that result's relevance.
///
/// A multiplier < 1.0 dampens (e.g. `_logs/2020/** → 0.3`); a multiplier > 1.0
/// boosts (e.g. `Projects/*/sync.md → 1.5`). The multiplier is clamped to
/// `[RANKING_MULTIPLIER_MIN, RANKING_MULTIPLIER_MAX]` on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobRule {
    pub glob: String,
    pub multiplier: f64,
}

/// `[ranking]` — query-independent path-glob priors on result relevance
/// (Feature F6).
///
/// `dampen` and `boost` are just two ordered lists of [`GlobRule`]s; the
/// distinction is documentation only (a `dampen` rule conventionally has a
/// multiplier < 1.0 and a `boost` rule > 1.0, but neither is enforced — both
/// are clamped to the same bounds). When several rules match a result, the
/// **last** matching rule wins (last-match-wins), with `dampen` rules ordered
/// before `boost` rules in the merged list. Empty config → no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingConfig {
    #[serde(default)]
    pub dampen: Vec<GlobRule>,
    #[serde(default)]
    pub boost: Vec<GlobRule>,
    /// Feature F7 (PRF half) — enable pseudo-relevance-feedback query
    /// expansion on brain BM25 searches. Off by default; when `true`, a
    /// two-pass term-mining expansion runs before fusion. The CLI `--prf`
    /// flag and MCP `prf: true` argument override this per call.
    #[serde(default)]
    pub enable_prf: bool,
    /// Substring patterns matched case-insensitively against a symbol's file
    /// path to deboost test/fixture code in `search_symbols_by_name` ranking.
    /// Override via `[ranking] test_path_patterns` in instance config.
    #[serde(default = "default_test_path_patterns")]
    pub test_path_patterns: Vec<String>,
    /// Feature F12 — git-activity-dampened CodeRank weight. Controls how
    /// strongly the per-file recency score rescales `pagerank_score` at read
    /// time via `clamp(1 + w*(score - 0.5), 0.4, 1.6)`.
    ///
    /// Default `1.2` (NOT `0.6`): with `score ∈ [0, 1]` the factor spans
    /// `[1 - w/2, 1 + w/2]`, so only `w = 1.2` reaches the full `[0.4, 1.6]`
    /// clamp; `0.6` would top out at `[0.7, 1.3]` and never bind the clamp.
    /// This is only applied when a `<db>.gitactivity.json` sidecar is present
    /// (populated via `index --with-git-activity`).
    #[serde(default = "default_git_activity_weight")]
    pub git_activity_weight: f64,
}

/// Default test-path deboost patterns for [`RankingConfig::test_path_patterns`].
///
/// These patterns are matched (case-insensitive substring) against a symbol's
/// file path during `search_symbols_by_name` ranking. Matching symbols are
/// demoted so production code ranks above test/fixture code of the same name.
pub fn default_test_path_patterns() -> Vec<String> {
    vec![
        "/playwright/".into(),
        "/__tests__/".into(),
        "/test/".into(),
        "/tests/".into(),
        "/e2e/".into(),
        "/fixtures/".into(),
        "/__mocks__/".into(),
        "/cypress/".into(),
        "/spec/".into(),
        "/it/".into(),
        "/itest/".into(),
        ".test.".into(),
        ".spec.".into(),
        ".cy.".into(),
        "_test.go".into(),
        "_spec.rb".into(),
    ]
}

/// Default for [`RankingConfig::git_activity_weight`]. See the field doc and
/// `nestweaver_engine::git_activity` for the clamp/weight rationale.
fn default_git_activity_weight() -> f64 {
    1.2
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            dampen: Vec::new(),
            boost: Vec::new(),
            enable_prf: false,
            test_path_patterns: default_test_path_patterns(),
            git_activity_weight: default_git_activity_weight(),
        }
    }
}

impl RankingConfig {
    /// Clamp every rule's multiplier into the allowed bounds. Called on load so
    /// downstream code can trust the values without re-validating.
    pub fn clamp_multipliers(&mut self) {
        for rule in self.dampen.iter_mut().chain(self.boost.iter_mut()) {
            rule.multiplier = rule
                .multiplier
                .clamp(RANKING_MULTIPLIER_MIN, RANKING_MULTIPLIER_MAX);
        }
    }

    /// Return the merged, ordered rule list used for last-match-wins matching:
    /// all `dampen` rules first (in declaration order) then all `boost` rules.
    pub fn ordered_rules(&self) -> Vec<&GlobRule> {
        self.dampen.iter().chain(self.boost.iter()).collect()
    }

    /// True when there are no rules at all (the off / no-op path).
    pub fn is_empty(&self) -> bool {
        self.dampen.is_empty() && self.boost.is_empty()
    }
}

/// Configuration for cross-domain link discovery (notes ↔ code bridging).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossDomainConfig {
    /// Additional stoplist words to suppress on top of the built-in list.
    #[serde(default)]
    pub stoplist_extend: Vec<String>,
    /// When set, completely replaces the built-in stoplist instead of
    /// extending it. Use with care — the built-in list is well-tuned.
    #[serde(default)]
    pub stoplist_replace: Option<Vec<String>>,
    /// Override the minimum symbol name length filter. Default: 4.
    #[serde(default)]
    pub min_symbol_name_length: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LinkConfig {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub link_type: String,
    pub description: Option<String>,
    pub endpoints: Option<Vec<String>>,
    pub identifiers: Option<Vec<String>>,
    pub contract: Option<String>,
    /// When `true`, insert a `CROSS_REPO_LINK` edge in the graph for every
    /// (Symbol in `from`-repo, Symbol in `to`-repo) pair that shares a name.
    /// Defaults to `false` — declared links are metadata-only unless opted in.
    #[serde(default)]
    pub materialize: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FeatureConfig {
    pub name: String,
    pub description: Option<String>,
    pub repos: Vec<String>,
    pub entry_points: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InstanceConfig {
    pub instance_id: String,
    /// Optional path to the graph database (`.lbug`) this instance reads.
    /// Lets `--config` select a DB so read commands don't also need `--db`.
    /// Absent → callers fall back to `--db` / `NESTWEAVER_DB` / the default.
    #[serde(default)]
    pub db: Option<String>,
    pub snapshot_storage: StorageConfig,
    pub workspace: WorkspaceConfig,
    pub inference: InferenceConfig,
    pub git: GitConfig,
    /// Repos this instance indexes. Optional so an empty server needs no `repos`
    /// line at all — avoids the shipped-template `repos = []` footgun where a
    /// later `[[repos]]` append would collide into a duplicate key. Populated
    /// configs use `[[repos]]` table-array blocks.
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
    pub schema_extensions: Option<SchemaExtensions>,
    pub links: Option<Vec<LinkConfig>>,
    pub features: Option<Vec<FeatureConfig>>,
    #[serde(default)]
    pub cross_domain: CrossDomainConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Feature F8 — tiered inline bodies. Controls when brain_context /
    /// brain_search may embed a result's source body inline.
    #[serde(default)]
    pub response: ResponseConfig,
    /// Feature F6 — per-path dampen/boost ranking priors. Query-independent
    /// multipliers on result relevance, keyed by file-path glob.
    #[serde(default)]
    pub ranking: RankingConfig,
    /// Finding #7 — graduated path-aware deboost + kind-aware tiebreak applied
    /// during `search_symbols_by_name` seed resolution. Replaces the binary
    /// `[ranking].test_path_patterns` mechanism (which remains parseable for
    /// one release as a deprecation shim).
    #[serde(default)]
    pub seed_resolution: SeedResolutionConfig,
    /// Feature F16 — response cache tuning (`[cache]`).
    #[serde(default)]
    pub cache: CacheConfig,
    /// Vault file-watching configuration (`[watch]`).
    #[serde(default)]
    pub watch: WatchConfig,
    /// Default pagination limits for tool responses (`[limits]`).
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Server-mode configuration (`[server]`).
    #[serde(default)]
    pub server: ServerConfig,
    /// Local embedding model and hybrid-search blend configuration (`[embedding]`).
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    /// Upstream NestWeaver servers (`[[upstream]]`). Parsed here so the
    /// sections are not silently ignored; the client crate re-reads them
    /// via its own discovery layer.
    #[serde(default)]
    pub upstream: Vec<UpstreamEntry>,
}

/// `[embedding]` — local embedding model and hybrid-search blend configuration.
///
/// Controls which sentence-transformer model is used for semantic search,
/// where the model weights are cached, and the BM25/PPR/semantic fusion weights
/// applied when blending result sets.
#[derive(Debug, Deserialize, Clone)]
pub struct EmbeddingConfig {
    /// HuggingFace model ID (or local path) for the sentence-transformer.
    /// Default: `"sentence-transformers/all-MiniLM-L6-v2"` (384-dim, fast). A DB's
    /// recorded embedding model overrides this at daemon startup.
    #[serde(default = "default_model_id")]
    pub model_id: String,
    /// Directory where downloaded model weights are stored.
    /// Default: `"~/.cache/nestweaver/models"`.
    #[serde(default = "default_embedding_cache_dir")]
    pub cache_dir: String,
    /// Optional HTTP endpoint for an external embedding service (e.g. Ollama,
    /// OpenAI-compatible). When set, the local model is not loaded.
    pub external_endpoint: Option<String>,
    /// Model name to pass to the external endpoint. Required when
    /// `external_endpoint` is set.
    pub external_model: Option<String>,
    /// Blend weight for PPR (Personalized PageRank) scores. Default 0.40.
    #[serde(default = "default_weight_ppr")]
    pub weight_ppr: f64,
    /// Blend weight for BM25 scores. Default 0.25.
    #[serde(default = "default_weight_bm25")]
    pub weight_bm25: f64,
    /// Blend weight for semantic (embedding cosine-similarity) scores. Default 0.35.
    #[serde(default = "default_weight_semantic")]
    pub weight_semantic: f64,
    /// When `true`, semantic scores are always mixed in even if the query
    /// matched zero BM25 results. Default `true`.
    #[serde(default = "default_true")]
    pub always_blend_semantic: bool,
    /// Maximum number of seeds passed to the semantic re-ranker. Default 5.
    #[serde(default = "default_semantic_seed_limit")]
    pub semantic_seed_limit: usize,
    /// Candidate pool size for the semantic ANN search. Default 200.
    #[serde(default = "default_semantic_search_limit")]
    pub semantic_search_limit: usize,
}

fn default_model_id() -> String {
    // all-MiniLM-L6-v2: 384-dim, ~22MB, mean-pooled, no prefix — fast and light, the best
    // general default for most users (many run CPU-only servers). A DB embedded with a
    // different model records it (set_embedding_metadata) and the daemon loads that instead,
    // so this default only applies to fresh/un-embedded instances. Override per-instance via
    // `[embedding] model_id` (e.g. thenlper/gte-base for higher-quality 768-dim retrieval).
    "sentence-transformers/all-MiniLM-L6-v2".to_string()
}
fn default_embedding_cache_dir() -> String {
    "~/.cache/nestweaver/models".to_string()
}
fn default_weight_ppr() -> f64 {
    0.40
}
fn default_weight_bm25() -> f64 {
    0.25
}
fn default_weight_semantic() -> f64 {
    0.35
}
fn default_semantic_seed_limit() -> usize {
    5
}
fn default_semantic_search_limit() -> usize {
    200
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model_id: default_model_id(),
            cache_dir: default_embedding_cache_dir(),
            external_endpoint: None,
            external_model: None,
            weight_ppr: default_weight_ppr(),
            weight_bm25: default_weight_bm25(),
            weight_semantic: default_weight_semantic(),
            always_blend_semantic: true,
            semantic_seed_limit: default_semantic_seed_limit(),
            semantic_search_limit: default_semantic_search_limit(),
        }
    }
}

/// `[cache]` — tuning for the F16 response cache (Feature F16).
///
/// The cache stores ZSTD-compressed responses of deterministic read tools in
/// a `<db>.cache` sidecar. Correctness is key-based: an entry only hits when
/// the persisted `graph_generation` and a filemeta scope digest both still
/// match, so a reindex invalidates everything WITHOUT a background daemon.
/// `max_size_mb` caps total stored size; LRU eviction trims the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Maximum total cache size in MiB before LRU eviction kicks in.
    /// Default 256.
    #[serde(default = "default_cache_max_size_mb")]
    pub max_size_mb: u64,
}

fn default_cache_max_size_mb() -> u64 {
    256
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_mb: default_cache_max_size_mb(),
        }
    }
}

/// `[watch]` — vault file-watching configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_true() -> bool {
    true
}
fn default_debounce_ms() -> u64 {
    200
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 200,
        }
    }
}

/// Default result limit for paginated MCP tool responses.
pub const DEFAULT_RESULT_LIMIT: usize = 50;

/// `[limits]` — default pagination limits for tool responses.
///
/// MCP tools read `default_result_limit` at dispatch time via the
/// thread-local `InstanceConfig` (set by the daemon or direct MCP server).
/// When no instance config is loaded, tools fall back to
/// `DEFAULT_RESULT_LIMIT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_result_limit")]
    pub default_result_limit: usize,
}

fn default_result_limit() -> usize {
    DEFAULT_RESULT_LIMIT
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            default_result_limit: DEFAULT_RESULT_LIMIT,
        }
    }
}

/// `[response]` — tuning for tiered inline bodies (Feature F8).
///
/// Inline bodies are off by default; the caller must opt in (CLI
/// `--inline-bodies`, MCP `include_bodies: true`). When opted in, a result's
/// body is embedded only if its normalized relevance clears
/// `inline_body_threshold`. Each body is truncated to `inline_max_body_tokens`
/// (chars/4 estimate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseConfig {
    /// Minimum normalized relevance (0.0–1.0) a result must reach before its
    /// body is embedded inline. Default 0.75.
    #[serde(default = "default_inline_body_threshold")]
    pub inline_body_threshold: f64,
    /// Per-body cap in estimated tokens (chars/4). Default 800.
    #[serde(default = "default_inline_max_body_tokens")]
    pub inline_max_body_tokens: usize,
}

fn default_inline_body_threshold() -> f64 {
    0.75
}

fn default_inline_max_body_tokens() -> usize {
    800
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            inline_body_threshold: default_inline_body_threshold(),
            inline_max_body_tokens: default_inline_max_body_tokens(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub backend: String,
    pub path: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceConfig {
    pub backend: String,
    pub path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InferenceConfig {
    pub endpoint: String,
    pub embedding_model: String,
    pub summary_model: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitConfig {
    pub credential_method: String,
}

/// How a repo's contents are indexed. `Code` (the default) runs the language
/// indexer producing Symbol/File nodes; `Vault` runs the markdown indexer
/// producing Note/Section/Heading nodes. Declared via `type = "code" | "vault"`
/// in a `[[repos]]` block — mirrors the `type` discriminator on [`LinkConfig`].
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepoType {
    Code,
    Vault,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RepoConfig {
    pub url: String,
    /// Index strategy for this repo. Absent → treated as [`RepoType::Code`].
    #[serde(rename = "type", default)]
    pub repo_type: Option<RepoType>,
    /// Optional repo display-name alias. When set, project and feature
    /// configs may refer to the repo by this name even if the DB-indexed
    /// repo is stored under a different display name (typically the
    /// URL-derived basename).
    #[serde(default)]
    pub name: Option<String>,
    pub sparse: Option<bool>,
    pub pin_sha: Option<String>,
    /// Feature F12 — per-repo opt-out for git-activity-dampened CodeRank.
    /// `None`/`Some(true)` → recency dampening applies when a sidecar exists;
    /// `Some(false)` → this repo never has its CodeRank dampened by git
    /// activity (e.g. a vendored/generated repo where commit recency is noise).
    pub use_git_activity: Option<bool>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub poll: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SchemaExtensions {
    pub extra_node_properties: Option<HashMap<String, HashMap<String, String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub vault_folder: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub wiki_sources: Vec<WikiSourceConfig>,
    #[serde(default)]
    pub external_refs: Vec<ExternalRefConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSourceConfig {
    pub label: String,
    pub mcp_server: String,
    pub tool: String,
    #[serde(default)]
    pub args: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalRefConfig {
    pub label: String,
    pub url: String,
    #[serde(default)]
    pub ref_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Timeout in seconds for tool calls to this server (default 30).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A single `[[upstream]]` entry in the instance config.
///
/// Mirrors the `UpstreamConfig` shape from `nestweaver-client` but lives in
/// the engine crate to avoid a circular dependency. The client crate
/// re-reads these entries via its own discovery layer at connect time.
#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamEntry {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_upstream_mode")]
    pub mode: String,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default = "default_upstream_timeout")]
    pub timeout: String,
    /// Path to CA certificate PEM file for self-signed TLS.
    #[serde(default)]
    pub ca_cert: Option<String>,
}

fn default_upstream_mode() -> String {
    "fallback".to_string()
}
fn default_upstream_timeout() -> String {
    "1s".to_string()
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfig {
    #[serde(default)]
    pub indexing: IndexingConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IndexingConfig {
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default = "default_min_poll", alias = "poll_min")]
    pub min_poll: String,
    #[serde(default = "default_max_poll", alias = "poll_max")]
    pub max_poll: String,
}

fn default_workers() -> usize {
    8
}
fn default_min_poll() -> String {
    "45s".to_string()
}
fn default_max_poll() -> String {
    "8h".to_string()
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            min_poll: default_min_poll(),
            max_poll: default_max_poll(),
        }
    }
}

pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, "ms")
    } else {
        let last = s.chars().last()?;
        (
            &s[..s.len() - 1],
            if last == 's' {
                "s"
            } else if last == 'm' {
                "m"
            } else if last == 'h' {
                "h"
            } else {
                return None;
            },
        )
    };
    let num: u64 = num_str.parse().ok()?;
    match unit {
        "ms" => Some(std::time::Duration::from_millis(num)),
        "s" => Some(std::time::Duration::from_secs(num)),
        "m" => Some(std::time::Duration::from_secs(num * 60)),
        "h" => Some(std::time::Duration::from_secs(num * 3600)),
        _ => None,
    }
}

impl InstanceConfig {
    /// The DB path declared by this instance, if any.
    pub fn db_path(&self) -> Option<std::path::PathBuf> {
        self.db.as_ref().map(std::path::PathBuf::from)
    }

    /// Parse an `InstanceConfig` from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, anyhow::Error> {
        let mut config: Self = toml::from_str(s)?;
        // Feature F6: clamp ranking-prior multipliers into bounds on load so
        // downstream code can trust the values without re-validating.
        config.ranking.clamp_multipliers();
        // Finding #7: validate seed-resolution rules, clamp factors, and
        // synthesize a deprecation shim from `[ranking].test_path_patterns`
        // when callers haven't moved to `[seed_resolution]` yet.
        validate_and_normalize_seed_resolution(&mut config.seed_resolution, &config.ranking)?;
        if config.inference.endpoint.is_empty() {
            anyhow::bail!("inference.endpoint must be set (no global default allowed)");
        }

        // Validate features and links — warn but don't fail.
        if let Some(features) = &config.features {
            for feature in features {
                if feature.repos.is_empty() {
                    tracing::warn!(
                        "feature '{}' has no repos declared — it will match nothing",
                        feature.name
                    );
                }
                if feature.entry_points.is_empty() {
                    tracing::warn!(
                        "feature '{}' has no entry_points declared — context will be empty",
                        feature.name
                    );
                }
            }
        }
        if let Some(links) = &config.links {
            for link in links {
                if link.from == link.to {
                    tracing::warn!(
                        "link from '{}' to '{}' has the same repo on both ends — this is likely a mistake",
                        link.from,
                        link.to
                    );
                }
            }
        }

        Ok(config)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, anyhow::Error> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_toml_str(&contents)
    }
}

/// Append a `[[repos]]` entry to an instance config file if it is not already
/// present. Returns `true` when the file was changed.
/// Remove a top-level `repos = []` (empty inline array) line so a subsequent
/// `[[repos]]` append doesn't collide with it into a duplicate `repos` key.
/// Only the EMPTY inline form is stripped (whitespace-insensitive: `repos=[]`,
/// `repos = [ ]`); non-empty inline arrays and `[[repos]]` blocks are preserved.
fn strip_empty_inline_repos(contents: &str) -> String {
    let is_empty_repos_line = |line: &str| {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("repos") else {
            return false;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            return false;
        };
        rest.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            == "[]"
    };
    let kept: Vec<&str> = contents
        .lines()
        .filter(|l| !is_empty_repos_line(l))
        .collect();
    let mut out = kept.join("\n");
    if contents.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn append_repo_to_config_file(
    path: &std::path::Path,
    url: &str,
    branch: Option<&str>,
) -> Result<bool, anyhow::Error> {
    let mut contents = std::fs::read_to_string(path)?;
    let cfg = InstanceConfig::from_toml_str(&contents)?;
    let target = crate::jobs::canonical_repo_id(url);
    if cfg
        .repos
        .iter()
        .any(|repo| crate::jobs::canonical_repo_id(&repo.url) == target)
    {
        return Ok(false);
    }

    // A config may declare an empty repo set as an inline `repos = []` (the
    // shipped template's form). Appending a `[[repos]]` table-array on top of
    // that inline key produces a DUPLICATE `repos` key and corrupts the file, so
    // strip the empty inline line first. `[[repos]]` blocks and non-empty inline
    // arrays are left untouched (multiple `[[repos]]` blocks coexist fine).
    contents = strip_empty_inline_repos(&contents);

    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("\n[[repos]]\n");
    contents.push_str("url = ");
    contents.push_str(&toml_basic_string(url)?);
    contents.push('\n');
    if let Some(branch) = branch {
        contents.push_str("branch = ");
        contents.push_str(&toml_basic_string(branch)?);
        contents.push('\n');
    }
    std::fs::write(path, contents)?;
    Ok(true)
}

/// Remove a `[[repos]]` entry from an instance config file by canonical repo
/// URL. Returns `true` when the file was changed.
pub fn remove_repo_from_config_file(
    path: &std::path::Path,
    url: &str,
) -> Result<bool, anyhow::Error> {
    let contents = std::fs::read_to_string(path)?;
    let target = crate::jobs::canonical_repo_id(url);
    let lines: Vec<&str> = contents.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut removed = false;
    let mut i = 0usize;

    while i < lines.len() {
        if lines[i].trim() == "[[repos]]" {
            let start = i;
            i += 1;
            while i < lines.len() {
                let trimmed = lines[i].trim();
                if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
                    break;
                }
                i += 1;
            }
            let block = lines[start..i].join("\n");
            if repo_block_matches_url(&block, &target)? {
                removed = true;
            } else {
                out.extend_from_slice(&lines[start..i]);
            }
        } else {
            out.push(lines[i]);
            i += 1;
        }
    }

    if removed {
        let mut next = out.join("\n");
        if contents.ends_with('\n') {
            next.push('\n');
        }
        std::fs::write(path, next)?;
    }
    Ok(removed)
}

fn repo_block_matches_url(block: &str, target: &str) -> Result<bool, anyhow::Error> {
    let value: toml::Value = toml::from_str(block)?;
    let Some(repos) = value.get("repos").and_then(|v| v.as_array()) else {
        return Ok(false);
    };
    let Some(url) = repos
        .first()
        .and_then(|repo| repo.get("url"))
        .and_then(|url| url.as_str())
    else {
        return Ok(false);
    };
    Ok(crate::jobs::canonical_repo_id(url) == target)
}

fn toml_basic_string(value: &str) -> Result<String, anyhow::Error> {
    Ok(serde_json::to_string(value)?)
}

/// Set of `SymbolKind` variant names accepted by [`SeedResolutionConfig::kind_priority`].
/// Kept in sync with the variants at `crates/nestweaver-schema/src/nodes.rs`.
const VALID_SYMBOL_KINDS: &[&str] = &[
    "Function",
    "Class",
    "Method",
    "Interface",
    "Trait",
    "Enum",
    "Module",
    "Extension",
    "Constant",
    "Property",
    "TypeAlias",
    "Variable",
];

/// Validate `[seed_resolution]` rules, clamp out-of-range factors, and
/// synthesize a deprecation shim from `[ranking].test_path_patterns` when
/// the new block is empty but the old field was customized.
fn validate_and_normalize_seed_resolution(
    seed_resolution: &mut SeedResolutionConfig,
    ranking: &RankingConfig,
) -> Result<(), anyhow::Error> {
    // 1) Validate exactly-one-of {prefix, suffix} per rule and clamp factors.
    for (idx, rule) in seed_resolution.path_deboost.iter_mut().enumerate() {
        match (&rule.prefix, &rule.suffix) {
            (Some(_), Some(_)) => anyhow::bail!(
                "[seed_resolution.path_deboost][{idx}]: rule must set exactly one of prefix or suffix (got both)"
            ),
            (None, None) => anyhow::bail!(
                "[seed_resolution.path_deboost][{idx}]: rule must set exactly one of prefix or suffix (got neither)"
            ),
            _ => {}
        }
        let original = rule.factor;
        rule.factor = rule
            .factor
            .clamp(SEED_PATH_FACTOR_MIN, SEED_PATH_FACTOR_MAX);
        if (rule.factor - original).abs() > f64::EPSILON {
            tracing::warn!(
                "[seed_resolution.path_deboost][{idx}]: factor {original} out of range [{SEED_PATH_FACTOR_MIN}, {SEED_PATH_FACTOR_MAX}]; clamped to {}",
                rule.factor
            );
        }
    }

    // 2) Validate every kind_priority entry against the SymbolKind variants.
    for kind_name in &seed_resolution.kind_priority {
        if !VALID_SYMBOL_KINDS.contains(&kind_name.as_str()) {
            anyhow::bail!(
                "[seed_resolution].kind_priority: unknown SymbolKind '{kind_name}' (valid: {})",
                VALID_SYMBOL_KINDS.join(", ")
            );
        }
    }

    // 3) Deprecation shim — translate [ranking].test_path_patterns into
    //    prefix rules when the new block is empty but the legacy field is
    //    customized. Keeps backward compatibility for one release.
    if seed_resolution.path_deboost.is_empty() {
        let defaults = default_test_path_patterns();
        if ranking.test_path_patterns != defaults && !ranking.test_path_patterns.is_empty() {
            tracing::warn!(
                "[ranking].test_path_patterns is deprecated; migrate to [seed_resolution].path_deboost for graduated multiplicative deboost"
            );
            seed_resolution.path_deboost = ranking
                .test_path_patterns
                .iter()
                .map(|pat| PathDeboostRule {
                    prefix: Some(pat.clone()),
                    suffix: None,
                    factor: 0.3,
                })
                .collect();
        }
    }
    Ok(())
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_TOML: &str = r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/repo"
"#;

    #[test]
    fn shipped_docker_instance_toml_is_valid() {
        // The instance.toml committed at the repo root is mounted by
        // docker-compose. It must parse and validate or the container fails to
        // start. Regression guard for "Docker deployment not ready".
        let shipped = include_str!("../../../instance.toml");
        let cfg = InstanceConfig::from_toml_str(shipped)
            .expect("shipped Docker instance.toml must be a valid InstanceConfig");
        assert_eq!(cfg.instance_id, "nestweaver-server");
        assert!(
            !cfg.inference.endpoint.is_empty(),
            "inference.endpoint must be set"
        );
    }

    #[test]
    fn parses_minimal_config() {
        let cfg = InstanceConfig::from_toml_str(MINIMAL_TOML).expect("should parse");
        assert_eq!(cfg.instance_id, "test-instance");
        assert_eq!(cfg.snapshot_storage.backend, "local");
        assert_eq!(cfg.workspace.path, "/tmp/workspace");
        assert_eq!(cfg.inference.endpoint, "http://localhost:8080");
        assert_eq!(cfg.inference.embedding_model, "text-embedding-3-small");
        assert_eq!(cfg.inference.summary_model, "gpt-4o-mini");
        assert_eq!(cfg.git.credential_method, "ssh");
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].url, "https://github.com/example/repo");
        assert!(cfg.schema_extensions.is_none());
    }

    // Bug #19: `--config` should let a command select its DB. The config
    // carries an optional `db` path; absent → None (backward compatible).
    #[test]
    fn parses_optional_db_path() {
        let cfg = InstanceConfig::from_toml_str(MINIMAL_TOML).expect("should parse");
        assert_eq!(
            cfg.db_path(),
            None,
            "absent db must stay None (backward compatible)"
        );

        let with_db = format!("db = \"/home/u/.nestweaver/main.lbug\"\n{MINIMAL_TOML}");
        let cfg2 = InstanceConfig::from_toml_str(&with_db).expect("should parse");
        assert_eq!(
            cfg2.db_path(),
            Some(std::path::PathBuf::from("/home/u/.nestweaver/main.lbug"))
        );
    }

    #[test]
    fn rejects_config_without_inference_endpoint() {
        let toml = r#"
instance_id = "test"

[snapshot_storage]
backend = "local"
path = "/tmp"

[workspace]
backend = "local"
path = "/tmp"

[inference]
endpoint = ""
embedding_model = "model"
summary_model = "model"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/repo"
"#;
        let result = InstanceConfig::from_toml_str(toml);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("inference.endpoint"),
            "error should mention inference.endpoint, got: {msg}"
        );
    }

    #[test]
    fn parses_schema_extensions() {
        let toml = r#"
instance_id = "test"

[snapshot_storage]
backend = "local"
path = "/tmp"

[workspace]
backend = "local"
path = "/tmp"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "model"
summary_model = "model"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/repo"

[schema_extensions.extra_node_properties.Symbol]
team_owner = "string"
deprecated = "bool"
"#;
        let cfg = InstanceConfig::from_toml_str(toml).expect("should parse");
        let ext = cfg.schema_extensions.expect("should have extensions");
        let props = ext
            .extra_node_properties
            .expect("should have extra_node_properties");
        let symbol_props = props.get("Symbol").expect("should have Symbol props");
        assert_eq!(
            symbol_props.get("team_owner").map(String::as_str),
            Some("string")
        );
        assert_eq!(
            symbol_props.get("deprecated").map(String::as_str),
            Some("bool")
        );
    }

    #[test]
    fn parses_links_and_features() {
        let toml = r#"
instance_id = "cross-repo-test"

[snapshot_storage]
backend = "local"
path = "/tmp"

[workspace]
backend = "local"
path = "/tmp"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "model"
summary_model = "model"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/app"

[[repos]]
url = "https://github.com/example/service"

[[links]]
from = "app"
to = "service"
type = "http-api"
description = "App calls service REST API"
endpoints = ["/api/data"]

[[links]]
from = "app"
to = "firmware"
type = "ble"
identifiers = ["6E400001-B5A3-F393-E0A9-E50E24DCCA9E"]

[[features]]
name = "data-sync"
description = "Data synchronization feature"
repos = ["app", "service"]
entry_points = ["syncData", "fetchRecords"]
"#;
        let cfg = InstanceConfig::from_toml_str(toml).expect("should parse");

        let links = cfg.links.expect("should have links");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].from, "app");
        assert_eq!(links[0].to, "service");
        assert_eq!(links[0].link_type, "http-api");
        assert_eq!(
            links[0].description.as_deref(),
            Some("App calls service REST API")
        );
        assert_eq!(
            links[0].endpoints.as_deref(),
            Some(["/api/data".to_string()].as_slice())
        );
        assert_eq!(links[1].link_type, "ble");
        assert!(links[1].identifiers.is_some());

        let features = cfg.features.expect("should have features");
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name, "data-sync");
        assert_eq!(features[0].repos, vec!["app", "service"]);
        assert_eq!(features[0].entry_points, vec!["syncData", "fetchRecords"]);
    }

    // Feature F6: `[ranking]` parses into dampen/boost lists and multipliers
    // are clamped to [0.05, 5.0] on load.
    #[test]
    fn parses_ranking_priors_and_clamps_multipliers() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[ranking]

[[ranking.dampen]]
glob = "_logs/2020/**"
multiplier = 0.3

[[ranking.dampen]]
glob = "archive/**"
multiplier = 0.0

[[ranking.boost]]
glob = "Projects/*/sync.md"
multiplier = 1.5

[[ranking.boost]]
glob = "critical/**"
multiplier = 100.0
"#
        );
        let cfg = InstanceConfig::from_toml_str(&toml).expect("should parse");
        assert_eq!(cfg.ranking.dampen.len(), 2);
        assert_eq!(cfg.ranking.boost.len(), 2);
        assert_eq!(cfg.ranking.dampen[0].glob, "_logs/2020/**");
        assert!((cfg.ranking.dampen[0].multiplier - 0.3).abs() < 1e-9);
        // 0.0 clamped up to the floor.
        assert!((cfg.ranking.dampen[1].multiplier - RANKING_MULTIPLIER_MIN).abs() < 1e-9);
        assert!((cfg.ranking.boost[0].multiplier - 1.5).abs() < 1e-9);
        // 100.0 clamped down to the ceiling.
        assert!((cfg.ranking.boost[1].multiplier - RANKING_MULTIPLIER_MAX).abs() < 1e-9);
    }

    #[test]
    fn ranking_defaults_to_empty_noop() {
        let cfg = InstanceConfig::from_toml_str(MINIMAL_TOML).expect("should parse");
        assert!(cfg.ranking.is_empty(), "absent [ranking] must be a no-op");
    }

    // Feature F12: git_activity_weight defaults to 1.2 (the clamp-fix value, not
    // the RFC's 0.6) and per-repo use_git_activity parses as an opt-out.
    #[test]
    fn git_activity_weight_defaults_to_1_2() {
        let cfg = InstanceConfig::from_toml_str(MINIMAL_TOML).expect("should parse");
        assert!(
            (cfg.ranking.git_activity_weight - 1.2).abs() < 1e-9,
            "default git_activity_weight must be 1.2, got {}",
            cfg.ranking.git_activity_weight
        );
    }

    #[test]
    fn git_activity_weight_override_and_repo_opt_out_parse() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[ranking]
git_activity_weight = 0.8

[[repos]]
url = "https://github.com/example/live"

[[repos]]
url = "https://github.com/example/vendored"
use_git_activity = false
"#
        );
        let cfg = InstanceConfig::from_toml_str(&toml).expect("should parse");
        assert!((cfg.ranking.git_activity_weight - 0.8).abs() < 1e-9);
        let live = cfg
            .repos
            .iter()
            .find(|r| r.url == "https://github.com/example/live")
            .expect("live repo");
        let vendored = cfg
            .repos
            .iter()
            .find(|r| r.url == "https://github.com/example/vendored")
            .expect("vendored repo");
        assert_eq!(live.use_git_activity, None);
        assert_eq!(vendored.use_git_activity, Some(false));
    }

    #[test]
    fn parses_per_repo_overrides() {
        let toml = r#"
instance_id = "test"

[snapshot_storage]
backend = "local"
path = "/tmp"

[workspace]
backend = "local"
path = "/tmp"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "model"
summary_model = "model"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/full"

[[repos]]
url = "https://github.com/example/sparse"
sparse = true
pin_sha = "deadbeef1234"
"#;
        let cfg = InstanceConfig::from_toml_str(toml).expect("should parse");
        assert_eq!(cfg.repos.len(), 2);

        let full = &cfg.repos[0];
        assert_eq!(full.url, "https://github.com/example/full");
        assert!(full.sparse.is_none());
        assert!(full.pin_sha.is_none());

        let sparse = &cfg.repos[1];
        assert_eq!(sparse.url, "https://github.com/example/sparse");
        assert_eq!(sparse.sparse, Some(true));
        assert_eq!(sparse.pin_sha.as_deref(), Some("deadbeef1234"));
    }

    // ── Finding #7 — [seed_resolution] config block ───────────────────────

    #[test]
    fn parses_default_seed_resolution() {
        let cfg = InstanceConfig::from_toml_str(MINIMAL_TOML).expect("should parse");
        // Defaults populate the full 19-rule path_deboost list (9 test-mirror
        // prefixes + 5 vendored test-runtime prefixes + 5 .test/.spec suffixes).
        assert_eq!(
            cfg.seed_resolution.path_deboost.len(),
            19,
            "default [seed_resolution] must populate 19 rules"
        );
        // First default rule deboosts playwright/ at 0.2.
        let first = &cfg.seed_resolution.path_deboost[0];
        assert_eq!(first.prefix.as_deref(), Some("/playwright/"));
        assert!((first.factor - 0.2).abs() < 1e-9);
        // kind_priority covers every SymbolKind variant.
        assert_eq!(
            cfg.seed_resolution.kind_priority,
            default_kind_priority(),
            "default kind_priority must equal default_kind_priority()"
        );
    }

    #[test]
    fn parses_explicit_seed_resolution() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[seed_resolution]
kind_priority = ["Function", "Class"]

[[seed_resolution.path_deboost]]
prefix = "/playwright/"
factor = 0.1

[[seed_resolution.path_deboost]]
suffix = ".spec.ts"
factor = 0.25
"#
        );
        let cfg = InstanceConfig::from_toml_str(&toml).expect("should parse");
        assert_eq!(cfg.seed_resolution.path_deboost.len(), 2);
        assert_eq!(
            cfg.seed_resolution.path_deboost[0].prefix.as_deref(),
            Some("/playwright/")
        );
        assert!((cfg.seed_resolution.path_deboost[0].factor - 0.1).abs() < 1e-9);
        assert_eq!(
            cfg.seed_resolution.path_deboost[1].suffix.as_deref(),
            Some(".spec.ts")
        );
        assert!((cfg.seed_resolution.path_deboost[1].factor - 0.25).abs() < 1e-9);
        assert_eq!(
            cfg.seed_resolution.kind_priority,
            vec!["Function".to_string(), "Class".to_string()]
        );
    }

    #[test]
    fn rejects_rule_with_both_prefix_and_suffix() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[[seed_resolution.path_deboost]]
prefix = "/playwright/"
suffix = ".spec.ts"
factor = 0.5
"#
        );
        let err = InstanceConfig::from_toml_str(&toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exactly one of prefix or suffix") && msg.contains("got both"),
            "error should reject both-fields rule, got: {msg}"
        );
    }

    #[test]
    fn rejects_rule_with_neither_prefix_nor_suffix() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[[seed_resolution.path_deboost]]
factor = 0.5
"#
        );
        let err = InstanceConfig::from_toml_str(&toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exactly one of prefix or suffix") && msg.contains("got neither"),
            "error should reject empty rule, got: {msg}"
        );
    }

    #[test]
    fn clamps_factor_out_of_range() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[[seed_resolution.path_deboost]]
prefix = "/playwright/"
factor = 15.0

[[seed_resolution.path_deboost]]
prefix = "/cypress/"
factor = -1.0
"#
        );
        let cfg = InstanceConfig::from_toml_str(&toml).expect("should parse");
        assert!((cfg.seed_resolution.path_deboost[0].factor - SEED_PATH_FACTOR_MAX).abs() < 1e-9);
        assert!((cfg.seed_resolution.path_deboost[1].factor - SEED_PATH_FACTOR_MIN).abs() < 1e-9);
    }

    #[test]
    fn rejects_unknown_kind_in_priority() {
        let toml = format!(
            r#"
{MINIMAL_TOML}

[seed_resolution]
kind_priority = ["NotARealKind"]
"#
        );
        let err = InstanceConfig::from_toml_str(&toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("NotARealKind") && msg.contains("unknown SymbolKind"),
            "error should reject unknown kind, got: {msg}"
        );
    }

    #[test]
    fn deprecation_shim_translates_legacy_test_path_patterns() {
        // User has customized [ranking].test_path_patterns but has no
        // [seed_resolution] block — shim must translate into path_deboost rules.
        let toml = format!(
            r#"
{MINIMAL_TOML}

[ranking]
test_path_patterns = ["/legacy-fixtures/", "/_old_e2e_/"]

[seed_resolution]
path_deboost = []
"#
        );
        let cfg = InstanceConfig::from_toml_str(&toml).expect("should parse");
        assert_eq!(cfg.seed_resolution.path_deboost.len(), 2);
        assert_eq!(
            cfg.seed_resolution.path_deboost[0].prefix.as_deref(),
            Some("/legacy-fixtures/")
        );
        // Shim translates with factor 0.3 each.
        assert!((cfg.seed_resolution.path_deboost[0].factor - 0.3).abs() < 1e-9);
        assert!((cfg.seed_resolution.path_deboost[1].factor - 0.3).abs() < 1e-9);
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(
            parse_duration("45s"),
            Some(std::time::Duration::from_secs(45))
        );
        assert_eq!(
            parse_duration("5m"),
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(
            parse_duration("8h"),
            Some(std::time::Duration::from_secs(28800))
        );
        assert_eq!(
            parse_duration("500ms"),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("bogus"), None);
    }

    #[test]
    fn repo_config_poll_deserializes() {
        let toml_str = r#"
            url = "https://github.com/org/repo"
            branch = "develop"
            poll = "never"
        "#;
        let cfg: RepoConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.branch.as_deref(), Some("develop"));
        assert_eq!(cfg.poll.as_deref(), Some("never"));
    }

    #[test]
    fn repo_config_defaults_without_new_fields() {
        let toml_str = r#"url = "https://github.com/org/repo""#;
        let cfg: RepoConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.branch.is_none());
        assert!(cfg.poll.is_none());
    }

    #[test]
    fn append_repo_to_config_file_adds_once_with_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.toml");
        std::fs::write(&path, MINIMAL_TOML).unwrap();

        let changed =
            append_repo_to_config_file(&path, "https://github.com/example/new", Some("main"))
                .unwrap();
        assert!(changed);
        let cfg = InstanceConfig::from_file(&path).unwrap();
        assert!(cfg.repos.iter().any(|repo| {
            repo.url == "https://github.com/example/new" && repo.branch.as_deref() == Some("main")
        }));

        let changed =
            append_repo_to_config_file(&path, "https://github.com/example/new/", Some("main"))
                .unwrap();
        assert!(!changed, "canonical duplicate should not be appended");
        let cfg = InstanceConfig::from_file(&path).unwrap();
        assert_eq!(
            cfg.repos
                .iter()
                .filter(|repo| crate::jobs::canonical_repo_id(&repo.url)
                    == crate::jobs::canonical_repo_id("https://github.com/example/new"))
                .count(),
            1
        );
    }

    #[test]
    fn append_repo_to_config_file_replaces_inline_empty_repos() {
        // The shipped template declares an empty repo set as inline `repos = []`.
        // Appending a `[[repos]]` block on top of it must NOT produce a duplicate
        // `repos` key — the file must stay parseable across repeated adds.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.toml");
        let template = r#"instance_id = "t"
repos = []
[snapshot_storage]
backend = "local"
path = "/tmp/s"
[workspace]
backend = "local"
path = "/tmp/w"
[inference]
endpoint = "http://x"
embedding_model = "m"
summary_model = "s"
[git]
credential_method = "ssh"
"#;
        std::fs::write(&path, template).unwrap();

        // First add: must succeed AND keep the file parseable.
        assert!(append_repo_to_config_file(&path, "https://github.com/example/one", None).unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("repos = []"),
            "inline empty repos must be stripped, got:\n{after}"
        );
        let cfg = InstanceConfig::from_file(&path).expect("file must still parse after first add");
        assert!(
            cfg.repos
                .iter()
                .any(|r| r.url == "https://github.com/example/one")
        );

        // Second add: previously failed with "duplicate key"; must now work.
        assert!(append_repo_to_config_file(&path, "https://github.com/example/two", None).unwrap());
        let cfg = InstanceConfig::from_file(&path).expect("file must still parse after second add");
        assert_eq!(cfg.repos.len(), 2, "both repos should be present");
    }

    #[test]
    fn config_parses_without_any_repos_field() {
        // `repos` is now optional (serde default) so an empty server needs no
        // `repos` line at all — the clean way to avoid the inline-array footgun.
        let no_repos = r#"instance_id = "t"
[snapshot_storage]
backend = "local"
path = "/tmp/s"
[workspace]
backend = "local"
path = "/tmp/w"
[inference]
endpoint = "http://x"
embedding_model = "m"
summary_model = "s"
[git]
credential_method = "ssh"
"#;
        let cfg = InstanceConfig::from_toml_str(no_repos).expect("config without repos must parse");
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn remove_repo_from_config_file_removes_only_matching_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.toml");
        std::fs::write(
            &path,
            format!(
                r#"{MINIMAL_TOML}

[[repos]]
url = "https://github.com/example/remove-me"
branch = "main"

[[repos]]
url = "https://github.com/example/keep-me"
"#
            ),
        )
        .unwrap();

        let removed =
            remove_repo_from_config_file(&path, "https://github.com/example/remove-me/").unwrap();
        assert!(removed);
        let cfg = InstanceConfig::from_file(&path).unwrap();
        assert!(
            !cfg.repos
                .iter()
                .any(|repo| repo.url == "https://github.com/example/remove-me")
        );
        assert!(
            cfg.repos
                .iter()
                .any(|repo| repo.url == "https://github.com/example/keep-me")
        );
    }

    #[test]
    fn server_indexing_config_defaults() {
        let cfg = IndexingConfig::default();
        assert_eq!(cfg.workers, 8);
        assert_eq!(cfg.min_poll, "45s");
        assert_eq!(cfg.max_poll, "8h");
    }
}

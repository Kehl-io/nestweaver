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
    /// Feature F16 — response cache tuning (`[cache]`).
    #[serde(default)]
    pub cache: CacheConfig,
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

#[derive(Debug, Deserialize, Clone)]
pub struct RepoConfig {
    pub url: String,
    pub sparse: Option<bool>,
    pub pin_sha: Option<String>,
    /// Feature F12 — per-repo opt-out for git-activity-dampened CodeRank.
    /// `None`/`Some(true)` → recency dampening applies when a sidecar exists;
    /// `Some(false)` → this repo never has its CodeRank dampened by git
    /// activity (e.g. a vendored/generated repo where commit recency is noise).
    pub use_git_activity: Option<bool>,
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
}

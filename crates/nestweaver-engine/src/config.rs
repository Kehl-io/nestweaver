use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
}

#[derive(Debug, Deserialize, Clone)]
pub struct SchemaExtensions {
    pub extra_node_properties: Option<HashMap<String, HashMap<String, String>>>,
}

impl InstanceConfig {
    /// Parse an `InstanceConfig` from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, anyhow::Error> {
        let config: Self = toml::from_str(s)?;
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

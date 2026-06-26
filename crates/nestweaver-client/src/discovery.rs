//! Upstream server discovery and configuration.
//!
//! Discovery order (first match wins):
//! 1. `NESTWEAVER_UPSTREAM` env var (simple URL)
//! 2. `.nestweaver/server.toml` (walk up from working directory)
//! 3. `~/.config/nestweaver/upstreams.toml` (user-level config)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Routing mode for an upstream server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// Query local first. If repo isn't indexed locally, query server.
    Fallback,
    /// Query both in parallel, merge results via RRF.
    Merge,
    /// Always query server. Local is only for uncommitted file overlay.
    Primary,
}

impl Default for RoutingMode {
    fn default() -> Self {
        Self::Fallback
    }
}

/// Configuration for a single upstream server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    /// Human-readable name (e.g., "acme").
    #[serde(default)]
    pub name: Option<String>,
    /// gRPC URL (e.g., "grpcs://nestweaver.acme.com:9378").
    pub url: String,
    /// Bearer token for authentication.
    #[serde(default)]
    pub token: Option<String>,
    /// Routing mode.
    #[serde(default)]
    pub mode: RoutingMode,
    /// Repo globs this upstream handles (e.g., ["acme/*"]).
    #[serde(default)]
    pub repos: Vec<String>,
    /// Per-query timeout in human-readable form (e.g., "1s").
    #[serde(default = "default_timeout")]
    pub timeout: String,
}

fn default_timeout() -> String {
    "1s".to_string()
}

// ---------------------------------------------------------------------------
// Internal TOML shapes
// ---------------------------------------------------------------------------

/// Parsed content of `.nestweaver/server.toml`.
#[derive(Debug, Deserialize)]
struct ServerToml {
    upstream: UpstreamSection,
}

#[derive(Debug, Deserialize)]
struct UpstreamSection {
    url: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    mode: RoutingMode,
}

/// Parsed content of `~/.config/nestweaver/upstreams.toml`.
#[derive(Debug, Deserialize, Serialize)]
struct UpstreamsToml {
    #[serde(default)]
    upstream: Vec<UpstreamConfig>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Discover upstream server configurations.
///
/// Discovery order (first match wins):
/// 1. `NESTWEAVER_UPSTREAM` env var
/// 2. `.nestweaver/server.toml` (walks up from `start_dir`)
/// 3. `~/.config/nestweaver/upstreams.toml`
///
/// Returns an empty `Vec` when no upstream is configured.
pub fn discover_upstreams(start_dir: &Path) -> Vec<UpstreamConfig> {
    // 1. Env var
    if let Ok(url) = std::env::var("NESTWEAVER_UPSTREAM") {
        let token = std::env::var("NESTWEAVER_TOKEN").ok();
        return vec![UpstreamConfig {
            name: Some("env".to_string()),
            url,
            token,
            repos: vec![],
            mode: RoutingMode::default(),
            timeout: default_timeout(),
        }];
    }

    // 2. Walk up from start_dir looking for .nestweaver/server.toml
    if let Some(cfg) = find_server_toml(start_dir) {
        return vec![cfg];
    }

    // 3. User config: ~/.config/nestweaver/upstreams.toml
    if let Some(configs) = load_user_upstreams() {
        if !configs.is_empty() {
            return configs;
        }
    }

    vec![]
}

/// Save a single upstream to `~/.config/nestweaver/upstreams.toml`.
///
/// Replaces an existing entry with the same URL, or appends.
pub fn save_upstream(config: &UpstreamConfig) -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("could not determine config directory")?;
    let nw_dir = config_dir.join("nestweaver");
    std::fs::create_dir_all(&nw_dir)?;
    let path = nw_dir.join("upstreams.toml");

    let mut upstreams = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        let parsed: UpstreamsToml =
            toml::from_str(&content).unwrap_or(UpstreamsToml { upstream: vec![] });
        parsed.upstream
    } else {
        vec![]
    };

    // Replace existing upstream with same URL, or append.
    if let Some(pos) = upstreams.iter().position(|u| u.url == config.url) {
        upstreams[pos] = config.clone();
    } else {
        upstreams.push(config.clone());
    }

    let toml_str = toml::to_string_pretty(&UpstreamsToml {
        upstream: upstreams,
    })?;
    std::fs::write(&path, toml_str)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Walk up from `start_dir` looking for `.nestweaver/server.toml`.
fn find_server_toml(start_dir: &Path) -> Option<UpstreamConfig> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(".nestweaver").join("server.toml");
        if candidate.is_file() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Ok(parsed) = toml::from_str::<ServerToml>(&content) {
                    return Some(UpstreamConfig {
                        name: None,
                        url: parsed.upstream.url,
                        token: parsed.upstream.token,
                        repos: vec![],
                        mode: parsed.upstream.mode,
                        timeout: default_timeout(),
                    });
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Load upstreams from `~/.config/nestweaver/upstreams.toml`.
fn load_user_upstreams() -> Option<Vec<UpstreamConfig>> {
    let config_dir = dirs::config_dir()?;
    let path = config_dir.join("nestweaver").join("upstreams.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    let parsed: UpstreamsToml = toml::from_str(&content).ok()?;
    Some(parsed.upstream)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // Serialize tests that touch env vars to avoid races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn discover_from_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Ensure no leftover state.
        unsafe {
            std::env::remove_var("NESTWEAVER_UPSTREAM");
            std::env::remove_var("NESTWEAVER_TOKEN");
            std::env::set_var("NESTWEAVER_UPSTREAM", "grpcs://test.example.com:9378");
            std::env::set_var("NESTWEAVER_TOKEN", "nw_secret");
        }

        let upstreams = discover_upstreams(Path::new("/nonexistent"));
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].url, "grpcs://test.example.com:9378");
        assert_eq!(upstreams[0].token.as_deref(), Some("nw_secret"));
        assert_eq!(upstreams[0].name.as_deref(), Some("env"));

        unsafe {
            std::env::remove_var("NESTWEAVER_UPSTREAM");
            std::env::remove_var("NESTWEAVER_TOKEN");
        }
    }

    #[test]
    fn discover_from_server_toml() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("NESTWEAVER_UPSTREAM") };

        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        let nw_dir = tmp.path().join("a").join(".nestweaver");
        fs::create_dir_all(&nw_dir).unwrap();
        fs::write(
            nw_dir.join("server.toml"),
            r#"
[upstream]
url = "grpcs://acme.example.com:9378"
token = "nw_tok"
"#,
        )
        .unwrap();

        let upstreams = discover_upstreams(&nested);
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].url, "grpcs://acme.example.com:9378");
        assert_eq!(upstreams[0].token.as_deref(), Some("nw_tok"));
        assert_eq!(upstreams[0].mode, RoutingMode::Fallback); // default
    }

    #[test]
    fn parse_upstreams_toml() {
        let input = r#"
[[upstream]]
name = "acme"
url = "grpcs://nestweaver.acme.com:9378"
token = "nw_abc123"
mode = "fallback"
repos = ["acme/*"]
timeout = "1s"

[[upstream]]
name = "partner"
url = "grpcs://nestweaver.partner.io:9378"
mode = "merge"
repos = ["partner/*", "shared/*"]
timeout = "2s"
"#;
        let parsed: UpstreamsToml = toml::from_str(input).unwrap();
        assert_eq!(parsed.upstream.len(), 2);

        assert_eq!(parsed.upstream[0].name.as_deref(), Some("acme"));
        assert_eq!(parsed.upstream[0].mode, RoutingMode::Fallback);
        assert_eq!(parsed.upstream[0].repos, vec!["acme/*"]);

        assert_eq!(parsed.upstream[1].name.as_deref(), Some("partner"));
        assert_eq!(parsed.upstream[1].mode, RoutingMode::Merge);
        assert_eq!(parsed.upstream[1].repos.len(), 2);
    }

    #[test]
    fn discovery_priority_order() {
        let _guard = ENV_LOCK.lock().unwrap();

        // Set up both env var AND server.toml — env var should win.
        let tmp = tempfile::tempdir().unwrap();
        let nw_dir = tmp.path().join(".nestweaver");
        fs::create_dir_all(&nw_dir).unwrap();
        fs::write(
            nw_dir.join("server.toml"),
            r#"
[upstream]
url = "grpcs://file-based.example.com:9378"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("NESTWEAVER_UPSTREAM", "grpcs://env-based.example.com:9378");
        }

        let upstreams = discover_upstreams(tmp.path());
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].url, "grpcs://env-based.example.com:9378");

        unsafe { std::env::remove_var("NESTWEAVER_UPSTREAM") };

        // Now env var is gone — should discover from file.
        let upstreams = discover_upstreams(tmp.path());
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].url, "grpcs://file-based.example.com:9378");
    }

    #[test]
    fn discover_returns_empty_when_nothing_configured() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("NESTWEAVER_UPSTREAM") };

        let tmp = tempfile::tempdir().unwrap();
        let upstreams = discover_upstreams(tmp.path());
        assert!(upstreams.is_empty());
    }

    #[test]
    fn routing_mode_defaults_to_fallback() {
        let mode: RoutingMode = Default::default();
        assert_eq!(mode, RoutingMode::Fallback);
    }

    #[test]
    fn routing_mode_deserialize_variants() {
        #[derive(Deserialize)]
        struct W {
            mode: RoutingMode,
        }
        let w: W = toml::from_str(r#"mode = "fallback""#).unwrap();
        assert_eq!(w.mode, RoutingMode::Fallback);
        let w: W = toml::from_str(r#"mode = "merge""#).unwrap();
        assert_eq!(w.mode, RoutingMode::Merge);
        let w: W = toml::from_str(r#"mode = "primary""#).unwrap();
        assert_eq!(w.mode, RoutingMode::Primary);
    }

    #[test]
    fn server_toml_minimal() {
        // Only the required `url` field — everything else has defaults.
        let input = r#"
[upstream]
url = "grpcs://minimal.example.com:9378"
"#;
        let parsed: ServerToml = toml::from_str(input).unwrap();
        assert_eq!(parsed.upstream.url, "grpcs://minimal.example.com:9378");
        assert!(parsed.upstream.token.is_none());
        assert_eq!(parsed.upstream.mode, RoutingMode::Fallback);
    }
}

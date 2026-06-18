pub mod local;
pub mod preprocess;

use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct EmbedConfig {
    pub model_id: String,
    pub cache_dir: PathBuf,
    pub external_endpoint: Option<String>,
    pub external_model: Option<String>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model_id: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            cache_dir: default_cache_dir(),
            external_endpoint: None,
            external_model: None,
        }
    }
}

fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("nestweaver")
        .join("models")
}

pub struct EmbedModel {
    local: local::LocalModel,
    /// Retained for Task 3 (external API fallback).
    #[allow(dead_code)]
    config: EmbedConfig,
}

impl EmbedModel {
    pub fn load(config: &EmbedConfig) -> Result<Self> {
        let local = local::LocalModel::load(config)?;
        Ok(Self {
            local,
            config: config.clone(),
        })
    }

    pub fn dimension(&self) -> usize {
        self.local.dimension()
    }

    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // External API will be added in Task 3 — for now, always use local
        self.local.embed(texts)
    }

    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let results = self.embed(&[query])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embed returned empty results"))
    }
}

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
    _config: EmbedConfig,
}

impl EmbedModel {
    pub fn load(_config: &EmbedConfig) -> Result<Self> {
        // Stub — will be implemented in Task 2 (local.rs) and Task 3 (external.rs)
        Ok(Self {
            _config: _config.clone(),
        })
    }

    pub fn dimension(&self) -> usize {
        384 // MiniLM default — will be dynamic in Task 2
    }

    pub fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        anyhow::bail!("EmbedModel not yet implemented — see Task 2")
    }

    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let results = self.embed(&[query])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embed returned empty results"))
    }
}

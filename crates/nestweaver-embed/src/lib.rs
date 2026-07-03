mod external;
pub mod local;
pub mod preprocess;

use std::path::PathBuf;

use anyhow::Result;

/// Check if Metal GPU acceleration is available on this machine.
pub fn is_metal_available() -> bool {
    #[cfg(feature = "metal")]
    {
        std::panic::catch_unwind(|| candle_core::Device::new_metal(0).is_ok()).unwrap_or(false)
    }
    #[cfg(not(feature = "metal"))]
    {
        false
    }
}

/// Return a human-readable description of available acceleration.
pub fn hardware_description() -> String {
    if is_metal_available() {
        "Metal GPU (Apple Silicon)".to_string()
    } else {
        "CPU only".to_string()
    }
}

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
            model_id: "thenlper/gte-base".to_string(),
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
        if let Some(ref endpoint) = self.config.external_endpoint {
            let model = self
                .config
                .external_model
                .as_deref()
                .unwrap_or("text-embedding-3-small");
            match external::embed_via_api(endpoint, model, texts) {
                Ok(embeddings) => return Ok(embeddings),
                Err(e) => {
                    tracing::warn!(
                        "External embedding API failed, falling back to local model: {e}"
                    );
                }
            }
        }
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

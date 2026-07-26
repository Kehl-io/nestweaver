mod external;
pub mod local;
pub mod preprocess;

use std::path::PathBuf;

use anyhow::Result;

/// Requested device-selection policy for the local embedding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePolicy {
    #[default]
    Auto,
    Metal,
    Cpu,
}

/// Device selected for a loaded local embedding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Metal,
    Cpu,
}

/// Embedding backend used by an [`EmbedModel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingBackendKind {
    Local,
    External,
}

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
    pub device_policy: DevicePolicy,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            // Keep in sync with nestweaver_engine::config::DEFAULT_EMBEDDING_MODEL_ID
            // (the canonical constant; this crate sits below nestweaver-engine in
            // the dependency graph, so it cannot reference it directly).
            model_id: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            cache_dir: default_cache_dir(),
            external_endpoint: None,
            external_model: None,
            device_policy: DevicePolicy::Auto,
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
    backend: EmbedBackend,
    config: EmbedConfig,
}

enum EmbedBackend {
    Local(local::LocalModel),
    External,
}

impl EmbedModel {
    pub fn load(config: &EmbedConfig) -> Result<Self> {
        let backend = if config.external_endpoint.is_some() {
            EmbedBackend::External
        } else {
            EmbedBackend::Local(local::LocalModel::load(config)?)
        };
        Ok(Self {
            backend,
            config: config.clone(),
        })
    }

    /// Dimension known at model-load time. External services do not provide a
    /// trustworthy dimension until they return an embedding.
    pub fn dimension(&self) -> Option<usize> {
        match &self.backend {
            EmbedBackend::Local(model) => Some(model.dimension()),
            EmbedBackend::External => None,
        }
    }

    /// Whether this model produces vectors via a configured external endpoint.
    pub fn uses_external_endpoint(&self) -> bool {
        self.backend_kind() == EmbeddingBackendKind::External
    }

    pub fn backend_kind(&self) -> EmbeddingBackendKind {
        match self.backend {
            EmbedBackend::Local(_) => EmbeddingBackendKind::Local,
            EmbedBackend::External => EmbeddingBackendKind::External,
        }
    }

    /// Selected local device, if this model has a local backend.
    pub fn device_kind(&self) -> Option<DeviceKind> {
        match &self.backend {
            EmbedBackend::Local(model) => Some(model.device_kind()),
            EmbedBackend::External => None,
        }
    }

    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        match &self.backend {
            EmbedBackend::Local(model) => model.embed(texts),
            EmbedBackend::External => {
                let endpoint = self
                    .config
                    .external_endpoint
                    .as_deref()
                    .expect("external backend requires an endpoint");
                let model = self
                    .config
                    .external_model
                    .as_deref()
                    .unwrap_or("text-embedding-3-small");
                external::embed_via_api(endpoint, model, texts)
            }
        }
    }

    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let results = self.embed(&[query])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embed returned empty results"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_failure_does_not_load_local() {
        let config = EmbedConfig {
            model_id: "definitely-not-a-local-model".to_string(),
            cache_dir: std::env::temp_dir().join("nestweaver-empty-embedding-cache"),
            external_endpoint: Some("http://127.0.0.1:9".to_string()),
            external_model: Some("test-embedding-model".to_string()),
            device_policy: DevicePolicy::Cpu,
        };

        let model = EmbedModel::load(&config)
            .expect("external backend must load without a local model or cache");
        assert_eq!(model.backend_kind(), EmbeddingBackendKind::External);
        assert_eq!(model.device_kind(), None);
        assert_eq!(model.dimension(), None);

        let err = model
            .embed(&["query"])
            .expect_err("a closed external endpoint must return its error");
        assert!(err.to_string().contains("embedding API"));
    }
}

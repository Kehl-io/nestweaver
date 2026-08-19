mod external;
pub mod local;
pub mod preprocess;

use std::path::PathBuf;

use anyhow::Result;
pub use local::{
    ArtifactMode, DenseArtifacts, MissingModelArtifactError, ModelArtifacts,
    resolve_model_artifacts,
};
use nestweaver_schema::EmbeddingPipelineV2;

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
    probe_metal_runtime().is_ok()
}

/// Whether this embedding crate was compiled with Candle's Metal backend.
pub const fn metal_compiled() -> bool {
    cfg!(feature = "metal")
}

/// Run a tiny Metal compute/readback probe without loading model artifacts.
///
/// Creating a Metal device alone does not prove kernels can compile and
/// execute. The affine operation forces a real GPU kernel and `to_vec1`
/// synchronizes/readbacks its result.
pub fn probe_metal_runtime() -> Result<()> {
    #[cfg(feature = "metal")]
    {
        std::panic::catch_unwind(|| -> Result<()> {
            let device = candle_core::Device::new_metal(0)?;
            let input = candle_core::Tensor::from_slice(&[1.0_f32, 2.0], 2, &device)?;
            let output = input.affine(2.0, 1.0)?;
            let values = output.to_vec1::<f32>()?;
            anyhow::ensure!(
                values == [3.0_f32, 5.0],
                "Metal compute/readback returned unexpected values: {values:?}"
            );
            Ok(())
        })
        .map_err(|_| anyhow::anyhow!("Metal runtime probe panicked"))?
    }
    #[cfg(not(feature = "metal"))]
    {
        anyhow::bail!("Metal support was not compiled into this binary")
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
            // Keep in sync with nestweaver_engine::config — both the
            // DEFAULT_EMBEDDING_MODEL_ID constant and the
            // default_embedding_cache_dir() default (this crate sits below
            // nestweaver-engine in the dependency graph, so it cannot
            // reference either directly).
            model_id: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            cache_dir: default_cache_dir(),
            external_endpoint: None,
            external_model: None,
        }
    }
}

fn default_cache_dir() -> PathBuf {
    #[cfg(not(windows))]
    const FALLBACK: &str = "/var/cache/nestweaver/models";
    #[cfg(windows)]
    const FALLBACK: &str = r"C:\ProgramData\nestweaver\models";

    // InstanceConfig persists this default as a UTF-8 String. Use the native
    // cache only when both sides can represent it exactly; never silently
    // replace non-UTF-8 bytes or fall back relative to a caller's CWD.
    let utf8_model_dir = |root: PathBuf| {
        let path = root.join("nestweaver").join("models");
        path.to_str().is_some().then_some(path)
    };
    dirs::cache_dir()
        .and_then(&utf8_model_dir)
        .or_else(|| dirs::home_dir().and_then(|home| utf8_model_dir(home.join(".cache"))))
        .unwrap_or_else(|| PathBuf::from(FALLBACK))
}

pub struct EmbedModel {
    backend: EmbedBackend,
    config: EmbedConfig,
}

enum EmbedBackend {
    Local(Box<local::LocalModel>),
    External,
}

impl EmbedModel {
    /// Load using the default automatic device-selection policy.
    pub fn load(config: &EmbedConfig) -> Result<Self> {
        Self::load_with_policy(config, DevicePolicy::Auto)
    }

    /// Load with an explicit device-selection policy for the local backend.
    /// The policy is ignored by an external backend, which never loads local
    /// inference as a fallback.
    pub fn load_with_policy(config: &EmbedConfig, policy: DevicePolicy) -> Result<Self> {
        Self::load_with_policy_and_artifact_mode(config, policy, ArtifactMode::DownloadMissing)
    }

    /// Load with explicit device and artifact-resolution policies.
    pub fn load_with_policy_and_artifact_mode(
        config: &EmbedConfig,
        policy: DevicePolicy,
        mode: ArtifactMode,
    ) -> Result<Self> {
        let backend = if config.external_endpoint.is_some() {
            EmbedBackend::External
        } else {
            EmbedBackend::Local(Box::new(
                local::LocalModel::load_with_policy_and_artifact_mode(config, policy, mode)?,
            ))
        };
        Ok(Self {
            backend,
            config: config.clone(),
        })
    }

    /// Embedding dimension, preserving the original public API. External
    /// backends have no trustworthy dimension at load time and return `0`.
    pub fn dimension(&self) -> usize {
        self.known_dimension().unwrap_or(0)
    }

    /// Dimension known at model-load time. External services do not provide a
    /// trustworthy dimension until they return an embedding.
    pub fn known_dimension(&self) -> Option<usize> {
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

    /// Complete semantic-space contract for vectors produced by this model.
    /// External providers reveal their dimension only in a response, so the
    /// caller supplies that observed value without inventing revision or
    /// preprocessing metadata.
    pub fn pipeline_for_dimension(&self, dimension: usize) -> Result<EmbeddingPipelineV2> {
        match &self.backend {
            EmbedBackend::Local(model) => {
                anyhow::ensure!(
                    model.dimension() == dimension,
                    "local embedding dimension changed from {} to {dimension}",
                    model.dimension()
                );
                Ok(model.pipeline().clone())
            }
            EmbedBackend::External => {
                let dimension = u32::try_from(dimension)
                    .map_err(|_| anyhow::anyhow!("embedding dimension does not fit u32"))?;
                anyhow::ensure!(dimension > 0, "external embedding dimension is unknown");
                let model = self
                    .config
                    .external_model
                    .as_deref()
                    .unwrap_or("text-embedding-3-small");
                let provider = self
                    .config
                    .external_endpoint
                    .as_deref()
                    .and_then(|endpoint| endpoint.split("://").nth(1))
                    .and_then(|authority| authority.split('/').next())
                    .unwrap_or("external");
                let mut pipeline = EmbeddingPipelineV2::external(provider, model, dimension);
                // The external transport returns provider vectors, then this
                // crate applies an explicit L2 normalization pass. Record that
                // observable NestWeaver policy even though provider revision
                // and preprocessing remain opaque.
                pipeline.normalize = Some(true);
                Ok(pipeline)
            }
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
    fn metal_capability_distinguishes_compile_support_from_runtime_probe() {
        assert_eq!(metal_compiled(), cfg!(feature = "metal"));
        if !metal_compiled() {
            let error = probe_metal_runtime()
                .expect_err("a build without Metal must not report a successful runtime probe");
            assert!(error.to_string().contains("not compiled"));
        }
    }

    #[test]
    fn external_failure_does_not_load_local() {
        let config = EmbedConfig {
            model_id: "definitely-not-a-local-model".to_string(),
            cache_dir: std::env::temp_dir().join("nestweaver-empty-embedding-cache"),
            external_endpoint: Some("http://127.0.0.1:9".to_string()),
            external_model: Some("test-embedding-model".to_string()),
        };

        let model = EmbedModel::load_with_policy(&config, DevicePolicy::Cpu)
            .expect("external backend must load without a local model or cache");
        assert_eq!(model.backend_kind(), EmbeddingBackendKind::External);
        assert_eq!(model.device_kind(), None);
        assert_eq!(model.dimension(), 0);
        assert_eq!(model.known_dimension(), None);

        let err = model
            .embed(&["query"])
            .expect_err("a closed external endpoint must return its error");
        assert!(err.to_string().contains("embedding API"));
    }

    #[test]
    fn external_backend_never_resolves_local_artifacts() {
        let config = EmbedConfig {
            model_id: "definitely-not-a-local-model".to_string(),
            cache_dir: std::env::temp_dir().join("nestweaver-missing-external-artifact-cache"),
            external_endpoint: Some("http://127.0.0.1:9".to_string()),
            external_model: Some("test-embedding-model".to_string()),
        };

        let model = EmbedModel::load_with_policy_and_artifact_mode(
            &config,
            DevicePolicy::Cpu,
            ArtifactMode::CacheOnly,
        )
        .expect("external backend must not inspect the local artifact cache");

        assert_eq!(model.backend_kind(), EmbeddingBackendKind::External);
    }
}

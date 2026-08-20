use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const EMBEDDING_PIPELINE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingBackend {
    SentenceTransformersLocal,
    ExternalProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPoolingMode {
    Cls,
    Max,
    Mean,
    MeanSqrtLength,
    WeightedMean,
    LastToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingSimilarity {
    Cosine,
    DotProduct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingTruncation {
    LongestFirst,
    ProviderDefined,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingQuantization {
    Float32,
}

/// Exact semantic-space contract. Equality means vectors may be compared;
/// model name and dimension alone deliberately do not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingPipelineV2 {
    pub schema_version: u32,
    pub backend: EmbeddingBackend,
    pub provider: String,
    pub model_id: String,
    /// Immutable Hub commit or provider revision, when observable.
    pub model_revision: Option<String>,
    pub weights_sha256: Option<String>,
    pub tokenizer_sha256: Option<String>,
    pub tokenizer_config_sha256: Option<String>,
    pub modules_sha256: Option<String>,
    pub produced_dimension: u32,
    pub projection_dimension: Option<u32>,
    pub pooling: Vec<EmbeddingPoolingMode>,
    pub include_prompt: Option<bool>,
    pub normalize: Option<bool>,
    pub similarity: EmbeddingSimilarity,
    pub max_sequence_length: Option<u32>,
    pub truncation: EmbeddingTruncation,
    pub quantization: EmbeddingQuantization,
}

impl EmbeddingPipelineV2 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != EMBEDDING_PIPELINE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported embedding pipeline schema {}",
                self.schema_version
            ));
        }
        if self.provider.trim().is_empty() || self.model_id.trim().is_empty() {
            return Err("embedding provider and model_id must be non-empty".to_string());
        }
        if self.produced_dimension == 0 {
            return Err("embedding produced_dimension must be non-zero".to_string());
        }
        if self.projection_dimension == Some(0) {
            return Err("embedding projection_dimension must be non-zero".to_string());
        }
        if matches!(self.backend, EmbeddingBackend::SentenceTransformersLocal) {
            if self.model_revision.as_deref().is_none_or(str::is_empty)
                || self.weights_sha256.as_deref().is_none_or(str::is_empty)
                || self.tokenizer_sha256.as_deref().is_none_or(str::is_empty)
                || self.modules_sha256.as_deref().is_none_or(str::is_empty)
            {
                return Err(
                    "local embedding pipeline requires immutable revision, weights, tokenizer, and modules identities"
                        .to_string(),
                );
            }
            if self.pooling.is_empty() || self.normalize.is_none() {
                return Err(
                    "local embedding pipeline requires declared pooling and normalization"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String, String> {
        self.validate()?;
        let canonical = serde_json::to_vec(self)
            .map_err(|error| format!("serialize embedding pipeline: {error}"))?;
        Ok(format!(
            "embedding-pipeline-v2:{}",
            hex::encode(Sha256::digest(canonical))
        ))
    }

    /// Honest compatibility projection for an opaque provider: revision and
    /// preprocessing fields remain unknown instead of being invented.
    pub fn external(provider: &str, model_id: &str, dimension: u32) -> Self {
        Self {
            schema_version: EMBEDDING_PIPELINE_SCHEMA_VERSION,
            backend: EmbeddingBackend::ExternalProvider,
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            model_revision: None,
            weights_sha256: None,
            tokenizer_sha256: None,
            tokenizer_config_sha256: None,
            modules_sha256: None,
            produced_dimension: dimension,
            projection_dimension: None,
            pooling: Vec::new(),
            include_prompt: None,
            normalize: None,
            similarity: EmbeddingSimilarity::Cosine,
            max_sequence_length: None,
            truncation: EmbeddingTruncation::ProviderDefined,
            quantization: EmbeddingQuantization::Float32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dimension_different_semantics_have_different_fingerprints() {
        let first = EmbeddingPipelineV2::external("provider", "model", 384);
        let mut second = first.clone();
        second.normalize = Some(true);
        assert_ne!(first.fingerprint().unwrap(), second.fingerprint().unwrap());
    }

    #[test]
    fn local_pipeline_refuses_unknown_reproducibility_fields() {
        let mut pipeline = EmbeddingPipelineV2::external("huggingface", "model", 384);
        pipeline.backend = EmbeddingBackend::SentenceTransformersLocal;
        assert!(
            pipeline
                .validate()
                .unwrap_err()
                .contains("immutable revision")
        );
    }
}

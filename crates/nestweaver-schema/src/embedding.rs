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

/// A field-level comparison safe for operator and machine diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingPipelineDifference {
    pub field: String,
    pub recorded: serde_json::Value,
    pub incoming: serde_json::Value,
}

impl EmbeddingPipelineV2 {
    /// Free-form identifiers are hashed: even a model ID or revision can contain
    /// an operator's local path or credentials. Closed enums and numbers are safe.
    pub fn differences(&self, incoming: &Self) -> Result<Vec<EmbeddingPipelineDifference>, String> {
        let old = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let new = serde_json::to_value(incoming).map_err(|e| e.to_string())?;
        let safe = |key: &str, value: &serde_json::Value| -> serde_json::Value {
            match key {
                "provider"
                | "model_id"
                | "model_revision"
                | "weights_sha256"
                | "tokenizer_sha256"
                | "tokenizer_config_sha256"
                | "modules_sha256"
                    if !value.is_null() =>
                {
                    serde_json::json!({"sha256": hex::encode(Sha256::digest(value.to_string().as_bytes()))})
                }
                _ => value.clone(),
            }
        };
        let mut differences = Vec::new();
        if let Some(fields) = old.as_object() {
            for (field, recorded) in fields {
                let incoming = &new[field];
                if recorded != incoming {
                    differences.push(EmbeddingPipelineDifference {
                        field: field.clone(),
                        recorded: safe(field, recorded),
                        incoming: safe(field, incoming),
                    });
                }
            }
        }
        Ok(differences)
    }

    pub fn mismatch_diagnostic(
        recorded: Option<&Self>,
        incoming: &Self,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "reason": if recorded.is_some() { "pipeline_mismatch" } else { "legacy_or_missing_pipeline" },
            "recorded_fingerprint": recorded.map(Self::fingerprint).transpose()?,
            "incoming_fingerprint": incoming.fingerprint()?,
            "differences": recorded.map(|r| r.differences(incoming)).transpose()?,
            "remediation": "use the recorded pipeline or intentionally rebuild all embeddings"
        }))
    }
}

#[cfg(test)]
mod hardening_diff_tests {
    use super::*;
    #[test]
    fn differences_are_complete_and_free_form_values_are_safe() {
        let old = EmbeddingPipelineV2::external("provider", "/private/token=secret", 2);
        let mut new = old.clone();
        new.produced_dimension = 4;
        new.normalize = Some(true);
        new.model_id = "/another/private/path".into();
        let diff = old.differences(&new).unwrap();
        assert_eq!(diff.len(), 3);
        let encoded = serde_json::to_string(&diff).unwrap();
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("secret"));
        assert!(encoded.contains("produced_dimension"));
        assert!(old.differences(&old).unwrap().is_empty());
        let legacy = EmbeddingPipelineV2::mismatch_diagnostic(None, &new).unwrap();
        assert_eq!(legacy["reason"], "legacy_or_missing_pipeline");
    }
}

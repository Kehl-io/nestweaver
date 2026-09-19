//! Self-describing envelopes for mutable graph-derived sidecars.
//!
//! Sealed publication bundles checksum whole files, but live sidecars also need
//! enough local metadata to reject a file copied from another brain, an older
//! graph generation, or an incompatible producer before decoding its payload.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Marks a generation-staleness refusal inside a [`StoreError`] message.
///
/// nw-289. Callers must be able to tell "this artifact describes an OLDER
/// generation of THIS graph" — rebuildable derived data, which a backup can
/// exclude and carry on — from "this artifact belongs to a DIFFERENT graph",
/// which is a corruption signal that must abort. Both arrive as
/// `StoreError::Query`, so the distinction lives in one constant both the
/// producer and every classifier read, rather than in a literal each side
/// spells for itself.
pub const STALE_ARTIFACT_MARKER: &str = "stale artifact generation";

/// Whether an error message is the generation-staleness refusal above.
#[must_use]
pub fn is_stale_artifact_generation(message: &str) -> bool {
    message.contains(STALE_ARTIFACT_MARKER)
}

/// Marks a refusal that a caller can clear by REBUILDING the artifact.
///
/// nw-459. `producer_version` is compared by exact string equality, so a
/// cache written by ANY earlier release compares unequal — even when
/// `algorithm_fingerprint`, the field that exists to express algorithmic
/// compatibility, matches exactly. That collapsed two different facts into
/// one refusal: "a different ALGORITHM produced this" (must abort) and "the
/// same algorithm in an older BUILD produced this" (drop it and move on).
///
/// Measured cost of the collapse on a live install: the first `brain refresh`
/// after upgrading exited 1, because a producer-only mismatch degraded a
/// publication whose graph had committed perfectly. Every user hit that on
/// every upgrade.
///
/// This marker restores the distinction the module header already claims to
/// draw, alongside [`STALE_ARTIFACT_MARKER`], so callers classify rather than
/// string-match a version.
pub const REBUILDABLE_ARTIFACT_MARKER: &str = "rebuildable artifact";

/// Whether an error message is a refusal a re-index would clear.
#[must_use]
pub fn is_rebuildable_artifact(message: &str) -> bool {
    message.contains(REBUILDABLE_ARTIFACT_MARKER) || is_stale_artifact_generation(message)
}

use serde_json::Value;

use crate::{PublicationIdentity, StoreError};

pub const ARTIFACT_ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactEnvelope {
    pub envelope_version: u32,
    pub artifact_kind: String,
    pub artifact_schema_version: u32,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub producer_version: String,
    pub source_graph_generation: u64,
    pub algorithm_fingerprint: String,
    /// The parameters the producer used, declared in the open.
    ///
    /// nw-147: `algorithm_fingerprint` is an opaque hash, so a reader with no
    /// independent expectation could only compare it to ITSELF — which always
    /// passes. Recording the inputs lets a reader RECOMPUTE the fingerprint
    /// and reject an artifact whose declared parameters do not produce the
    /// fingerprint it carries. An artifact can then no longer vouch for its
    /// own provenance.
    ///
    /// Shape is per artifact kind; `null` for kinds that declare nothing yet.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub algorithm_parameters: Value,
    pub payload_blake3: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct ArtifactExpectation<'a> {
    pub artifact_kind: &'a str,
    pub artifact_schema_version: u32,
    pub identity: &'a PublicationIdentity,
    pub producer_version: &'a str,
    pub source_graph_generation: u64,
    pub algorithm_fingerprint: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactRejection {
    #[error("stale artifact generation {actual}, expected {expected}; regenerate derived data")]
    StaleGeneration { actual: u64, expected: u64 },
    #[error(
        "rebuildable artifact: producer {actual}, expected {expected}; regenerate derived data"
    )]
    ProducerChanged { actual: String, expected: String },
    #[error("foreign artifact identity")]
    ForeignIdentity,
    #[error("corrupt artifact {0}")]
    Corrupt(String),
    #[error("incompatible artifact: {0}")]
    Incompatible(String),
}

impl ArtifactEnvelope {
    pub fn new<T: Serialize>(
        expectation: ArtifactExpectation<'_>,
        payload: &T,
    ) -> Result<Self, StoreError> {
        expectation.identity.validate()?;
        validate_expectation(expectation)?;
        let payload =
            canonical_value(serde_json::to_value(payload).map_err(|error| {
                StoreError::Query(format!("serialize artifact payload: {error}"))
            })?);
        let payload_blake3 = payload_digest(&payload)?;
        Ok(Self {
            envelope_version: ARTIFACT_ENVELOPE_VERSION,
            artifact_kind: expectation.artifact_kind.to_string(),
            artifact_schema_version: expectation.artifact_schema_version,
            brain_uuid: expectation.identity.brain_uuid.clone(),
            publication_uuid: expectation.identity.publication_uuid.clone(),
            producer_version: expectation.producer_version.to_string(),
            source_graph_generation: expectation.source_graph_generation,
            algorithm_fingerprint: expectation.algorithm_fingerprint.to_string(),
            algorithm_parameters: Value::Null,
            payload_blake3,
            payload,
        })
    }

    /// Declare the parameters that produced this artifact's fingerprint.
    ///
    /// nw-147: a reader can then recompute the fingerprint from these and
    /// refuse an artifact whose declaration does not match what it carries.
    #[must_use]
    pub fn with_algorithm_parameters(mut self, parameters: Value) -> Self {
        self.algorithm_parameters = parameters;
        self
    }

    pub fn validate_and_decode<T: DeserializeOwned>(
        &self,
        expectation: ArtifactExpectation<'_>,
    ) -> Result<T, StoreError> {
        self.validate_and_decode_typed(expectation)
            .map_err(|error| StoreError::Query(error.to_string()))
    }

    /// Integrity and identity take precedence over temporal invalidation. An
    /// old producer or generation cannot conceal damaged or foreign bytes.
    pub fn validate_and_decode_typed<T: DeserializeOwned>(
        &self,
        expectation: ArtifactExpectation<'_>,
    ) -> Result<T, ArtifactRejection> {
        use ArtifactRejection::*;
        expectation
            .identity
            .validate()
            .map_err(|e| Incompatible(e.to_string()))?;
        validate_expectation(expectation).map_err(|e| Incompatible(e.to_string()))?;
        if self.envelope_version != ARTIFACT_ENVELOPE_VERSION
            || self.artifact_kind != expectation.artifact_kind
            || self.artifact_schema_version != expectation.artifact_schema_version
            || self.algorithm_fingerprint != expectation.algorithm_fingerprint
        {
            return Err(Incompatible(
                "envelope version, kind, schema or algorithm mismatch".into(),
            ));
        }
        let observed_identity = PublicationIdentity {
            brain_uuid: self.brain_uuid.clone(),
            publication_uuid: self.publication_uuid.clone(),
        };
        observed_identity
            .validate()
            .map_err(|e| Corrupt(e.to_string()))?;
        if !uuid_equal(&self.brain_uuid, &expectation.identity.brain_uuid)
            .map_err(|e| Corrupt(e.to_string()))?
            || !uuid_equal(
                &self.publication_uuid,
                &expectation.identity.publication_uuid,
            )
            .map_err(|e| Corrupt(e.to_string()))?
        {
            return Err(ForeignIdentity);
        }
        let digest = payload_digest(&canonical_value(self.payload.clone()))
            .map_err(|e| Corrupt(e.to_string()))?;
        if digest != self.payload_blake3 {
            return Err(Corrupt("payload checksum mismatch".into()));
        }
        let payload = serde_json::from_value(self.payload.clone())
            .map_err(|e| Corrupt(format!("decode artifact payload: {e}")))?;
        if self.source_graph_generation != expectation.source_graph_generation {
            return Err(StaleGeneration {
                actual: self.source_graph_generation,
                expected: expectation.source_graph_generation,
            });
        }
        if self.producer_version != expectation.producer_version {
            return Err(ProducerChanged {
                actual: self.producer_version.clone(),
                expected: expectation.producer_version.into(),
            });
        }
        Ok(payload)
    }
}

fn validate_expectation(expectation: ArtifactExpectation<'_>) -> Result<(), StoreError> {
    if expectation.artifact_kind.is_empty()
        || expectation.artifact_schema_version == 0
        || expectation.producer_version.is_empty()
        || expectation.algorithm_fingerprint.is_empty()
    {
        return Err(StoreError::Query(
            "artifact expectation requires non-empty kind, producer, and fingerprint plus a non-zero schema version"
                .to_string(),
        ));
    }
    Ok(())
}

fn uuid_equal(left: &str, right: &str) -> Result<bool, StoreError> {
    let left = uuid::Uuid::parse_str(left)
        .map_err(|error| StoreError::Query(format!("invalid artifact UUID '{left}': {error}")))?;
    let right = uuid::Uuid::parse_str(right)
        .map_err(|error| StoreError::Query(format!("invalid expected UUID '{right}': {error}")))?;
    Ok(left == right)
}

fn payload_digest(payload: &Value) -> Result<String, StoreError> {
    let encoded = serde_json::to_vec(payload).map_err(|error| {
        StoreError::Query(format!("serialize canonical artifact payload: {error}"))
    })?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

fn canonical_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_value).collect()),
        Value::Object(values) => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| (key, canonical_value(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn identity() -> PublicationIdentity {
        PublicationIdentity {
            brain_uuid: uuid::Uuid::new_v4().to_string(),
            publication_uuid: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn expectation(identity: &PublicationIdentity) -> ArtifactExpectation<'_> {
        ArtifactExpectation {
            artifact_kind: "ranking",
            artifact_schema_version: 2,
            identity,
            producer_version: "6.2.0",
            source_graph_generation: 12,
            algorithm_fingerprint: "pagerank-v2",
        }
    }

    #[test]
    fn envelope_round_trips_canonical_payload() {
        let identity = identity();
        let payload = HashMap::from([("b".to_string(), 0.2), ("a".to_string(), 0.8)]);
        let envelope = ArtifactEnvelope::new(expectation(&identity), &payload).unwrap();
        let decoded: HashMap<String, f64> = envelope
            .validate_and_decode(expectation(&identity))
            .unwrap();
        assert_eq!(decoded, payload);
    }

    /// nw-459: a producer-version bump alone must NOT read as "incompatible".
    ///
    /// `producer_version` was compared by exact string equality, so a cache
    /// written by ANY earlier release was rejected — even though
    /// `algorithm_fingerprint`, the field that exists to express algorithmic
    /// compatibility, matched. Measured cost on a live install: the first
    /// `brain refresh` after upgrading exited 1 on a vault that had committed
    /// perfectly.
    #[test]
    fn a_producer_version_bump_alone_is_rebuildable_not_incompatible() {
        let identity = identity();
        let payload = HashMap::from([("a".to_string(), 1.0)]);
        let envelope = ArtifactEnvelope::new(expectation(&identity), &payload).unwrap();

        let mut upgraded = expectation(&identity);
        upgraded.producer_version = "9.3.0";
        let error = envelope
            .validate_and_decode::<HashMap<String, f64>>(upgraded)
            .unwrap_err()
            .to_string();

        assert!(
            is_rebuildable_artifact(&error),
            "a producer-only difference must be classified rebuildable so callers can \
             drop the cache and carry on; got {error:?}"
        );
        assert!(
            error.contains("re-index") || error.contains("regenerate"),
            "and it must name a remedy, like the generation check does; got {error:?}"
        );
    }

    /// COUNTERWEIGHT. Only the producer may differ. A fingerprint or schema
    /// change is a REAL incompatibility and must stay a hard refusal, or this
    /// fix would silently accept an artifact computed by a different algorithm.
    #[test]
    fn a_fingerprint_or_schema_change_is_still_hard_incompatible() {
        let identity = identity();
        let payload = HashMap::from([("a".to_string(), 1.0)]);
        let envelope = ArtifactEnvelope::new(expectation(&identity), &payload).unwrap();

        let mut fingerprint = expectation(&identity);
        fingerprint.producer_version = "9.3.0";
        fingerprint.algorithm_fingerprint = "pagerank-v3";
        let error = envelope
            .validate_and_decode::<HashMap<String, f64>>(fingerprint)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incompatible artifact"), "got {error:?}");
        assert!(
            !is_rebuildable_artifact(&error),
            "a different ALGORITHM is not merely rebuildable; got {error:?}"
        );

        let mut schema = expectation(&identity);
        schema.producer_version = "9.3.0";
        schema.artifact_schema_version += 1;
        let error = envelope
            .validate_and_decode::<HashMap<String, f64>>(schema)
            .unwrap_err()
            .to_string();
        assert!(!is_rebuildable_artifact(&error), "got {error:?}");
    }

    #[test]
    fn envelope_rejects_foreign_stale_incompatible_and_corrupt_content() {
        let graph_identity = identity();
        let payload = HashMap::from([("a".to_string(), 1.0)]);
        let envelope = ArtifactEnvelope::new(expectation(&graph_identity), &payload).unwrap();

        let foreign = identity();
        assert!(
            envelope
                .validate_and_decode::<HashMap<String, f64>>(expectation(&foreign))
                .unwrap_err()
                .to_string()
                .contains("foreign artifact identity")
        );
        let mut stale = expectation(&graph_identity);
        stale.source_graph_generation += 1;
        assert!(
            envelope
                .validate_and_decode::<HashMap<String, f64>>(stale)
                .unwrap_err()
                .to_string()
                .contains("stale artifact generation")
        );
        let mut incompatible = expectation(&graph_identity);
        incompatible.artifact_schema_version += 1;
        assert!(
            envelope
                .validate_and_decode::<HashMap<String, f64>>(incompatible)
                .unwrap_err()
                .to_string()
                .contains("incompatible artifact")
        );
        let mut corrupt = envelope;
        corrupt.payload = serde_json::json!({"a": 0.5});
        assert!(
            corrupt
                .validate_and_decode::<HashMap<String, f64>>(expectation(&graph_identity))
                .unwrap_err()
                .to_string()
                .contains("corrupt artifact payload checksum")
        );
    }
    #[test]
    fn temporal_mismatch_never_masks_integrity_or_foreign_identity() {
        let identity = identity();
        let mut envelope =
            ArtifactEnvelope::new(expectation(&identity), &serde_json::json!({"ok": true}))
                .unwrap();
        envelope.payload = serde_json::json!({"ok": false});
        let mut stale = expectation(&identity);
        stale.source_graph_generation += 1;
        assert!(matches!(
            envelope.validate_and_decode_typed::<serde_json::Value>(stale),
            Err(ArtifactRejection::Corrupt(_))
        ));
        let mut envelope =
            ArtifactEnvelope::new(expectation(&identity), &serde_json::json!({"ok": true}))
                .unwrap();
        envelope.producer_version = "old".into();
        envelope.brain_uuid = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            envelope.validate_and_decode_typed::<serde_json::Value>(expectation(&identity)),
            Err(ArtifactRejection::ForeignIdentity)
        ));
    }
}

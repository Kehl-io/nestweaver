//! Self-describing envelopes for mutable graph-derived sidecars.
//!
//! Sealed publication bundles checksum whole files, but live sidecars also need
//! enough local metadata to reject a file copied from another brain, an older
//! graph generation, or an incompatible producer before decoding its payload.

use serde::Serialize;
use serde::de::DeserializeOwned;
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
        expectation.identity.validate()?;
        validate_expectation(expectation)?;
        if self.envelope_version != ARTIFACT_ENVELOPE_VERSION {
            return Err(incompatible(format!(
                "envelope version {} does not match supported version {}",
                self.envelope_version, ARTIFACT_ENVELOPE_VERSION
            )));
        }
        if self.artifact_kind != expectation.artifact_kind
            || self.artifact_schema_version != expectation.artifact_schema_version
            || self.producer_version != expectation.producer_version
            || self.algorithm_fingerprint != expectation.algorithm_fingerprint
        {
            return Err(incompatible(format!(
                "metadata is {}/{}/producer {}/fingerprint {}, expected {}/{}/producer {}/fingerprint {}",
                self.artifact_kind,
                self.artifact_schema_version,
                self.producer_version,
                self.algorithm_fingerprint,
                expectation.artifact_kind,
                expectation.artifact_schema_version,
                expectation.producer_version,
                expectation.algorithm_fingerprint
            )));
        }
        let observed_identity = PublicationIdentity {
            brain_uuid: self.brain_uuid.clone(),
            publication_uuid: self.publication_uuid.clone(),
        };
        observed_identity
            .validate()
            .map_err(|error| StoreError::Query(format!("corrupt artifact identity: {error}")))?;
        if !uuid_equal(&self.brain_uuid, &expectation.identity.brain_uuid)?
            || !uuid_equal(
                &self.publication_uuid,
                &expectation.identity.publication_uuid,
            )?
        {
            return Err(StoreError::Query(format!(
                "foreign artifact identity {}/{}, expected {}/{}",
                self.brain_uuid,
                self.publication_uuid,
                expectation.identity.brain_uuid,
                expectation.identity.publication_uuid
            )));
        }
        if self.source_graph_generation != expectation.source_graph_generation {
            return Err(StoreError::Query(format!(
                "stale artifact generation {}, expected {}",
                self.source_graph_generation, expectation.source_graph_generation
            )));
        }
        let observed_digest = payload_digest(&canonical_value(self.payload.clone()))?;
        if observed_digest != self.payload_blake3 {
            return Err(StoreError::Query(format!(
                "corrupt artifact payload checksum: recorded {}, observed {}",
                self.payload_blake3, observed_digest
            )));
        }
        serde_json::from_value(self.payload.clone())
            .map_err(|error| StoreError::Query(format!("decode artifact payload: {error}")))
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

fn incompatible(message: String) -> StoreError {
    StoreError::Query(format!("incompatible artifact: {message}"))
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
}

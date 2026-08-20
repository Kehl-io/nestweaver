use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::{CancelReason, StoreError};
use crate::ranking::SeedResolutionConfig;

#[derive(Debug)]
struct RankedEmbedding {
    uid: String,
    score: f64,
}

impl PartialEq for RankedEmbedding {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.uid == other.uid
    }
}

impl Eq for RankedEmbedding {}

impl PartialOrd for RankedEmbedding {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedEmbedding {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.uid.cmp(&self.uid))
    }
}

fn retain_top(
    heap: &mut BinaryHeap<Reverse<RankedEmbedding>>,
    candidate: RankedEmbedding,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(Reverse(candidate));
    } else if heap.peek().is_some_and(|worst| candidate > worst.0) {
        heap.pop();
        heap.push(Reverse(candidate));
    }
}

fn finish_top(heap: BinaryHeap<Reverse<RankedEmbedding>>) -> Vec<(String, f64)> {
    let mut ranked: Vec<_> = heap.into_iter().map(|Reverse(item)| item).collect();
    ranked.sort_by(|left, right| right.cmp(left));
    ranked
        .into_iter()
        .map(|item| (item.uid, item.score))
        .collect()
}

fn merge_top(
    mut left: BinaryHeap<Reverse<RankedEmbedding>>,
    right: BinaryHeap<Reverse<RankedEmbedding>>,
    limit: usize,
) -> BinaryHeap<Reverse<RankedEmbedding>> {
    for Reverse(candidate) in right {
        retain_top(&mut left, candidate, limit);
    }
    left
}

fn embedding_similarity(
    embedding: &[f32],
    query: &[f32],
    query_norm: f64,
    similarity: &nestweaver_schema::EmbeddingSimilarity,
) -> f64 {
    let (dot, vector_norm_squared) = embedding.iter().zip(query).fold(
        (0.0_f64, 0.0_f64),
        |(dot, norm), (embedding_value, query_value)| {
            let embedding_value = f64::from(*embedding_value);
            (
                dot + embedding_value * f64::from(*query_value),
                norm + embedding_value * embedding_value,
            )
        },
    );
    match similarity {
        nestweaver_schema::EmbeddingSimilarity::Cosine => {
            let denominator = query_norm * vector_norm_squared.sqrt();
            if denominator == 0.0 {
                0.0
            } else {
                dot / denominator
            }
        }
        nestweaver_schema::EmbeddingSimilarity::DotProduct => dot,
    }
}

// ---------------------------------------------------------------------------
// EmbeddingIndex
// ---------------------------------------------------------------------------

/// In-memory embedding index backed by a binary sidecar file.
///
/// Binary format (v1):
/// ```text
/// [header: 16 bytes]
///   magic: b"NWEM" (4 bytes)
///   version: u32 LE (4 bytes) = 1
///   dimension: u32 LE (4 bytes)
///   count: u32 LE (4 bytes)
/// [uid table: count entries]
///   uid_len: u16 LE (2 bytes)
///   uid: [u8; uid_len]
/// [vectors: count * dimension * 4 bytes]
///   Contiguous f32 LE array, one vector per row
/// ```
enum EmbeddingBaseBytes {
    Mapped(memmap2::Mmap),
    Owned(Arc<[u8]>),
}

impl AsRef<[u8]> for EmbeddingBaseBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped(mapped) => mapped,
            Self::Owned(owned) => owned,
        }
    }
}

struct EmbeddingBaseRow {
    uid: String,
    vector_offset: usize,
}

struct EmbeddingBase {
    bytes: EmbeddingBaseBytes,
    rows: Vec<EmbeddingBaseRow>,
    row_by_uid: HashMap<String, usize>,
    dimension: usize,
}

impl EmbeddingBase {
    fn contains(&self, uid: &str) -> bool {
        self.row_by_uid.contains_key(uid)
    }

    fn vector_at(&self, row: usize) -> Vec<f32> {
        let row = &self.rows[row];
        let bytes = self.bytes.as_ref();
        (0..self.dimension)
            .map(|column| {
                let offset = row.vector_offset + column * 4;
                f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
            })
            .collect()
    }

    fn vector_bytes_at(&self, row: usize) -> &[u8] {
        let row = &self.rows[row];
        let byte_len = self.dimension * std::mem::size_of::<f32>();
        &self.bytes.as_ref()[row.vector_offset..row.vector_offset + byte_len]
    }

    fn score_at(
        &self,
        row: usize,
        query: &[f32],
        query_norm: f64,
        similarity: &nestweaver_schema::EmbeddingSimilarity,
    ) -> f64 {
        let row = &self.rows[row];
        let bytes = self.bytes.as_ref();
        let (dot, vector_norm_squared) = query.iter().enumerate().fold(
            (0.0_f64, 0.0_f64),
            |(dot, norm), (column, query_value)| {
                let offset = row.vector_offset + column * 4;
                let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                let value = f64::from(value);
                (dot + value * f64::from(*query_value), norm + value * value)
            },
        );
        match similarity {
            nestweaver_schema::EmbeddingSimilarity::Cosine => {
                let denominator = query_norm * vector_norm_squared.sqrt();
                if denominator == 0.0 {
                    0.0
                } else {
                    dot / denominator
                }
            }
            nestweaver_schema::EmbeddingSimilarity::DotProduct => dot,
        }
    }
}

#[derive(Clone, Copy)]
enum EmbeddingVectorRef<'a> {
    Overlay(&'a [f32]),
    Base(&'a [u8]),
}

impl EmbeddingVectorRef<'_> {
    fn dimension(self) -> usize {
        match self {
            Self::Overlay(vector) => vector.len(),
            Self::Base(bytes) => bytes.len() / std::mem::size_of::<f32>(),
        }
    }

    fn update_hasher(self, hasher: &mut blake3::Hasher) {
        match self {
            Self::Overlay(vector) => {
                for value in vector {
                    hasher.update(&value.to_le_bytes());
                }
            }
            Self::Base(bytes) => {
                hasher.update(bytes);
            }
        }
    }

    fn write_to(self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        match self {
            Self::Overlay(vector) => {
                for value in vector {
                    writer.write_all(&value.to_le_bytes())?;
                }
                Ok(())
            }
            Self::Base(bytes) => writer.write_all(bytes),
        }
    }
}

struct EmbeddingSnapshotEntry<'a> {
    uid: &'a str,
    vector: EmbeddingVectorRef<'a>,
}

pub struct EmbeddingIndex {
    /// Vectors created or replaced since the mapped base generation.
    embeddings: HashMap<String, Vec<f32>>,
    /// Immutable v2 base. File loads map its vector matrix instead of copying
    /// every vector into Rust heap allocations.
    base: Option<EmbeddingBase>,
    /// Base rows hidden by journal deletions. Overlay upserts shadow base rows
    /// with the same UID without adding them here.
    deleted_base_uids: HashSet<String>,
    /// Whether a `force` add has already cleared this index for a dimension
    /// switch. A model switch flips the dimension exactly once; a second flip
    /// in the same run means the embedding source is emitting mixed dimensions
    /// (e.g. a mid-run fallback to a different model) — clearing again would
    /// wipe everything embedded so far, so those vectors are rejected instead.
    /// Reset via [`reset_force_guard`] at the start of each embed run.
    ///
    /// [`reset_force_guard`]: EmbeddingIndex::reset_force_guard
    force_cleared: bool,
    /// The model id recorded in the database's embedding metadata, loaded once
    /// by `GraphStore` at open (and refreshed when new metadata is stamped) —
    /// never read per-add. `None` means unknown (never stamped, or unreadable),
    /// and unknown always allows the write: the dimension guard still applies.
    recorded_model_id: Option<String>,
    recorded_pipeline_fingerprint: Option<String>,
    similarity: nestweaver_schema::EmbeddingSimilarity,
    artifact_envelope: Option<EmbeddingArtifactEnvelopeV2>,
    pending_deltas: Vec<EmbeddingDelta>,
    journal_sequence: u64,
    journal_valid_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EmbeddingDelta {
    Clear,
    Upsert { uid: String },
    Delete { uid: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddingJournalPayload {
    sequence: u64,
    brain_uuid: String,
    publication_uuid: String,
    pipeline_fingerprint: String,
    delta: EmbeddingJournalDelta,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EmbeddingJournalDelta {
    Clear,
    Upsert { uid: String, vector: Vec<f32> },
    Delete { uid: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddingJournalRecord {
    payload: EmbeddingJournalPayload,
    checksum: String,
}

const EMBEDDING_JOURNAL_MAGIC: &[u8; 8] = b"NWJ2\0\0\0\x02";
const EMBEDDING_JOURNAL_COMPACT_BYTES: u64 = 16 * 1024 * 1024;
const EMBEDDING_JOURNAL_COMPACT_RECORDS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingArtifactEnvelopeV2 {
    pub schema_version: u32,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub source_graph_generation: u64,
    pub producer_version: String,
    pub pipeline: nestweaver_schema::EmbeddingPipelineV2,
    pub count: u64,
    pub dimension: u32,
    pub uid_table_bytes: u64,
    pub vector_bytes: u64,
    pub payload_blake3: String,
}

impl EmbeddingArtifactEnvelopeV2 {
    pub fn algorithm_fingerprint(&self) -> Result<String, anyhow::Error> {
        self.pipeline.fingerprint().map_err(anyhow::Error::msg)
    }
}

impl Default for EmbeddingIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingIndex {
    pub fn new() -> Self {
        Self {
            embeddings: HashMap::new(),
            base: None,
            deleted_base_uids: HashSet::new(),
            force_cleared: false,
            recorded_model_id: None,
            recorded_pipeline_fingerprint: None,
            similarity: nestweaver_schema::EmbeddingSimilarity::Cosine,
            artifact_envelope: None,
            pending_deltas: Vec::new(),
            journal_sequence: 0,
            journal_valid_bytes: None,
        }
    }

    /// Record the model id the database's embedding metadata names as the
    /// producer of this index's vectors. Called once by `GraphStore` at open
    /// and again whenever new metadata is stamped; `None` marks the producer
    /// as unknown, which disables the model guard (writes are then guarded by
    /// dimension only).
    pub fn set_recorded_model_id(&mut self, model_id: Option<String>) {
        self.recorded_model_id = model_id;
    }

    pub fn set_recorded_pipeline_fingerprint(&mut self, fingerprint: Option<String>) {
        self.recorded_pipeline_fingerprint = fingerprint;
    }

    pub fn set_similarity(&mut self, similarity: nestweaver_schema::EmbeddingSimilarity) {
        self.similarity = similarity;
    }

    pub fn artifact_envelope(&self) -> Option<&EmbeddingArtifactEnvelopeV2> {
        self.artifact_envelope.as_ref()
    }

    #[must_use = "a false return means a pipeline or dimension guard rejected the embedding"]
    pub fn add_with_pipeline(
        &mut self,
        uid: &str,
        embedding: Vec<f32>,
        pipeline: &nestweaver_schema::EmbeddingPipelineV2,
        force: bool,
    ) -> bool {
        let incoming = match pipeline.fingerprint() {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                tracing::warn!(uid, %error, "rejecting embedding from invalid pipeline");
                return false;
            }
        };
        if embedding.len() != pipeline.produced_dimension as usize {
            tracing::warn!(
                uid,
                got = embedding.len(),
                declared = pipeline.produced_dimension,
                "rejecting embedding whose vector dimension disagrees with its pipeline"
            );
            return false;
        }
        if let Some(recorded) = &self.recorded_pipeline_fingerprint
            && recorded != &incoming
        {
            // Metadata on an empty index is only an intended producer, not an
            // occupied semantic space. The first accepted vector may replace
            // it without `--force`; no mixed vectors can result.
            if self.is_empty() {
                self.recorded_pipeline_fingerprint = Some(incoming.clone());
                self.recorded_model_id = Some(pipeline.model_id.clone());
                self.similarity = pipeline.similarity.clone();
            } else if !force {
                tracing::warn!(
                    uid,
                    recorded,
                    incoming,
                    "rejecting embedding pipeline mismatch"
                );
                return false;
            } else if !self.force_cleared {
                self.embeddings.clear();
                self.base = None;
                self.deleted_base_uids.clear();
                self.pending_deltas.push(EmbeddingDelta::Clear);
                self.force_cleared = true;
            } else {
                tracing::warn!(
                    uid,
                    recorded,
                    incoming,
                    "rejecting a second pipeline switch in one embedding run"
                );
                return false;
            }
        }
        let accepted = self.add_with_model(uid, embedding, Some(&pipeline.model_id), force);
        if accepted {
            self.recorded_pipeline_fingerprint = Some(incoming);
            self.recorded_model_id = Some(pipeline.model_id.clone());
            self.similarity = pipeline.similarity.clone();
        }
        accepted
    }

    /// Insert an embedding. Returns `true` if accepted, `false` if rejected
    /// due to a dimension mismatch (when `force` is false, or when `force`
    /// already cleared the index once this run).
    ///
    /// When `force` is true and the incoming dimension differs from what's
    /// already stored, the entire index is cleared on the first mismatch so
    /// the new dimension becomes authoritative — this is the model-switch path.
    /// The clear happens at most once per run (see `force_cleared`).
    ///
    /// This entry point names no producing model, so the model guard is
    /// skipped (unknown producer); embed runs should use [`add_with_model`].
    ///
    /// [`add_with_model`]: EmbeddingIndex::add_with_model
    #[must_use = "a false return means the dimension guard rejected the embedding"]
    pub fn add(&mut self, uid: &str, embedding: Vec<f32>, force: bool) -> bool {
        self.add_with_model(uid, embedding, None, force)
    }

    /// Like [`add`], but also refuses a vector produced by a different model
    /// than the one recorded for this index, even at the same dimension:
    /// contrastively trained spaces share no basis, so mixing two models'
    /// vectors makes the index unusable for retrieval. The rejection names
    /// both model ids. `force` allows the switch (a `--force` run re-embeds
    /// everything, so no mixture survives). A `None` recorded or incoming
    /// model id is unknown and always allowed — the dimension guard still
    /// applies.
    ///
    /// [`add`]: EmbeddingIndex::add
    #[must_use = "a false return means a guard rejected the embedding"]
    pub fn add_with_model(
        &mut self,
        uid: &str,
        embedding: Vec<f32>,
        model_id: Option<&str>,
        force: bool,
    ) -> bool {
        if let (Some(recorded), Some(incoming)) = (self.recorded_model_id.as_deref(), model_id)
            && recorded != incoming
            && !force
        {
            tracing::warn!(
                uid,
                recorded,
                incoming,
                "skipping embedding from a different model than the one recorded for this \
                 index (re-embed with --force to switch models)"
            );
            return false;
        }
        if let Some(existing_dimension) = self.dimension()
            && embedding.len() != existing_dimension
        {
            if force && !self.force_cleared {
                tracing::info!(
                    old_dim = existing_dimension,
                    new_dim = embedding.len(),
                    "dimension change detected with --force; clearing index for model switch"
                );
                self.embeddings.clear();
                self.base = None;
                self.deleted_base_uids.clear();
                self.pending_deltas.push(EmbeddingDelta::Clear);
                self.force_cleared = true;
            } else if force {
                tracing::warn!(
                    uid,
                    got = embedding.len(),
                    expected = existing_dimension,
                    "rejecting embedding: index was already force-cleared once this run; \
                     the embedding source is emitting mixed dimensions"
                );
                return false;
            } else {
                tracing::warn!(
                    uid,
                    got = embedding.len(),
                    expected = existing_dimension,
                    "skipping embedding with mismatched dimension (re-embed with --force to switch models)"
                );
                return false;
            }
        }
        self.deleted_base_uids.remove(uid);
        self.embeddings.insert(uid.to_string(), embedding);
        self.pending_deltas.push(EmbeddingDelta::Upsert {
            uid: uid.to_string(),
        });
        true
    }

    /// Re-arm the force-clear guard. Call at the start of an embed run so a
    /// long-lived index (the daemon's) can honor a later, separate model
    /// switch while still refusing mixed dimensions within one run.
    pub fn reset_force_guard(&mut self) {
        self.force_cleared = false;
    }

    pub(crate) fn clear(&mut self) {
        self.embeddings.clear();
        self.base = None;
        self.deleted_base_uids.clear();
        self.artifact_envelope = None;
        self.pending_deltas.clear();
    }

    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let embeddings: HashMap<_, _> = self
            .all_uids()
            .into_iter()
            .filter_map(|uid| self.get(&uid).map(|vector| (uid, vector)))
            .collect();
        let json = serde_json::to_string(&embeddings)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, anyhow::Error> {
        let json = std::fs::read_to_string(path)?;
        let embeddings: HashMap<String, Vec<f32>> = serde_json::from_str(&json)?;
        Ok(Self {
            embeddings,
            base: None,
            deleted_base_uids: HashSet::new(),
            force_cleared: false,
            recorded_model_id: None,
            recorded_pipeline_fingerprint: None,
            similarity: nestweaver_schema::EmbeddingSimilarity::Cosine,
            artifact_envelope: None,
            pending_deltas: Vec::new(),
            journal_sequence: 0,
            journal_valid_bytes: None,
        })
    }

    // -- Binary persistence -------------------------------------------------

    /// Write the index in the compact binary sidecar format.
    pub fn save_binary(&self, path: &Path) -> Result<(), anyhow::Error> {
        use std::io::Write;
        let dim = self.dimension().unwrap_or(0);
        let mut entries: Vec<(String, Vec<f32>)> = self
            .all_uids()
            .into_iter()
            .filter_map(|uid| self.get(&uid).map(|vector| (uid, vector)))
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let count = entries.len() as u32;

        atomic_replace_file(path, |raw_file| {
            let mut file = std::io::BufWriter::new(raw_file);

            // Header
            file.write_all(b"NWEM")?;
            file.write_all(&1u32.to_le_bytes())?; // version
            file.write_all(&(dim as u32).to_le_bytes())?;
            file.write_all(&count.to_le_bytes())?;

            // UID table
            for (uid, _) in &entries {
                let bytes = uid.as_bytes();
                file.write_all(&(bytes.len() as u16).to_le_bytes())?;
                file.write_all(bytes)?;
            }

            // Vectors (contiguous f32 LE)
            for (_, vec) in &entries {
                for &val in vec.iter() {
                    file.write_all(&val.to_le_bytes())?;
                }
            }

            file.flush()
        })
    }

    /// Read the index from the compact binary sidecar format.
    pub fn load_binary(path: &Path) -> Result<Self, anyhow::Error> {
        let data = std::fs::read(path)?;
        if data.len() < 16 {
            anyhow::bail!("embedding file too small");
        }
        if &data[0..4] != b"NWEM" {
            anyhow::bail!("invalid embedding file magic");
        }
        let version = u32::from_le_bytes(data[4..8].try_into()?);
        if version != 1 {
            anyhow::bail!("unsupported embedding file version {version}");
        }
        let dim = u32::from_le_bytes(data[8..12].try_into()?) as usize;
        let count = u32::from_le_bytes(data[12..16].try_into()?) as usize;

        let mut offset = 16;
        let mut uids = Vec::with_capacity(count);

        // Read UID table
        for _ in 0..count {
            if offset + 2 > data.len() {
                anyhow::bail!("truncated uid table");
            }
            let uid_len = u16::from_le_bytes(data[offset..offset + 2].try_into()?) as usize;
            offset += 2;
            if offset + uid_len > data.len() {
                anyhow::bail!("truncated uid");
            }
            let uid = std::str::from_utf8(&data[offset..offset + uid_len])?.to_string();
            offset += uid_len;
            uids.push(uid);
        }

        // Read vectors
        let vec_bytes = count * dim * 4;
        if offset + vec_bytes > data.len() {
            anyhow::bail!("truncated vectors");
        }

        let mut embeddings = HashMap::with_capacity(count);
        for (i, uid) in uids.into_iter().enumerate() {
            let start = offset + i * dim * 4;
            let vec: Vec<f32> = (0..dim)
                .map(|j| {
                    f32::from_le_bytes(data[start + j * 4..start + j * 4 + 4].try_into().unwrap())
                })
                .collect();
            embeddings.insert(uid, vec);
        }

        Ok(Self {
            embeddings,
            base: None,
            deleted_base_uids: HashSet::new(),
            force_cleared: false,
            recorded_model_id: None,
            recorded_pipeline_fingerprint: None,
            similarity: nestweaver_schema::EmbeddingSimilarity::Cosine,
            artifact_envelope: None,
            pending_deltas: Vec::new(),
            journal_sequence: 0,
            journal_valid_bytes: None,
        })
    }

    /// Write the self-describing v2 vector artifact. The payload is
    /// deterministic and checksummed as one UID-table/vector snapshot.
    pub fn save_binary_v2(
        &self,
        path: &Path,
        identity: &crate::PublicationIdentity,
        source_graph_generation: u64,
        pipeline: &nestweaver_schema::EmbeddingPipelineV2,
    ) -> Result<EmbeddingArtifactEnvelopeV2, anyhow::Error> {
        use std::io::Write;
        pipeline.validate().map_err(anyhow::Error::msg)?;
        let entries = self.snapshot_entries();
        let dimension = self.dimension().unwrap_or(0);
        anyhow::ensure!(
            entries.is_empty() || dimension == pipeline.produced_dimension as usize,
            "embedding pipeline dimension {} does not match vector dimension {dimension}",
            pipeline.produced_dimension
        );
        let mut uid_table = Vec::new();
        for entry in &entries {
            anyhow::ensure!(
                entry.uid.len() <= u16::MAX as usize,
                "embedding UID is too long"
            );
            anyhow::ensure!(
                entry.vector.dimension() == dimension,
                "embedding dimensions are mixed"
            );
            uid_table.extend_from_slice(&(entry.uid.len() as u16).to_le_bytes());
            uid_table.extend_from_slice(entry.uid.as_bytes());
        }
        let vector_bytes = entries
            .len()
            .checked_mul(dimension)
            .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
            .ok_or_else(|| anyhow::anyhow!("embedding vector payload size overflow"))?;
        let mut payload_hasher = blake3::Hasher::new();
        payload_hasher.update(&uid_table);
        for entry in &entries {
            entry.vector.update_hasher(&mut payload_hasher);
        }
        let envelope = EmbeddingArtifactEnvelopeV2 {
            schema_version: 2,
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            source_graph_generation,
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            pipeline: pipeline.clone(),
            count: entries.len() as u64,
            dimension: dimension as u32,
            uid_table_bytes: uid_table.len() as u64,
            vector_bytes: vector_bytes as u64,
            payload_blake3: payload_hasher.finalize().to_hex().to_string(),
        };
        let encoded = serde_json::to_vec(&envelope)?;
        atomic_replace_file(path, |raw| {
            let mut file = std::io::BufWriter::new(raw);
            file.write_all(b"NWE2")?;
            file.write_all(&2_u32.to_le_bytes())?;
            file.write_all(&(encoded.len() as u64).to_le_bytes())?;
            file.write_all(&encoded)?;
            file.write_all(&uid_table)?;
            for entry in &entries {
                entry.vector.write_to(&mut file)?;
            }
            file.flush()
        })?;
        Ok(envelope)
    }

    pub fn load_binary_v2(path: &Path) -> Result<Self, anyhow::Error> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the mapping is read-only and retained by the returned index.
        // Canonical base replacement always uses rename, so an existing map
        // continues to reference its immutable generation rather than bytes
        // being changed underneath it.
        let mapped = unsafe { memmap2::MmapOptions::new().map(&file)? };
        Self::load_binary_v2_storage(EmbeddingBaseBytes::Mapped(mapped))
    }

    pub fn load_binary_v2_bytes(data: &[u8]) -> Result<Self, anyhow::Error> {
        Self::load_binary_v2_storage(EmbeddingBaseBytes::Owned(Arc::from(data)))
    }

    fn load_binary_v2_storage(storage: EmbeddingBaseBytes) -> Result<Self, anyhow::Error> {
        let data = storage.as_ref();
        anyhow::ensure!(data.len() >= 16, "embedding v2 file too small");
        anyhow::ensure!(
            &data[0..4] == b"NWE2",
            "embedding artifact requires full re-embed into v2"
        );
        let version = u32::from_le_bytes(data[4..8].try_into()?);
        anyhow::ensure!(version == 2, "unsupported embedding file version {version}");
        let envelope_len = u64::from_le_bytes(data[8..16].try_into()?) as usize;
        anyhow::ensure!(
            16 + envelope_len <= data.len(),
            "truncated embedding v2 envelope"
        );
        let envelope: EmbeddingArtifactEnvelopeV2 =
            serde_json::from_slice(&data[16..16 + envelope_len])?;
        anyhow::ensure!(envelope.schema_version == 2, "unsupported embedding schema");
        envelope.pipeline.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            envelope.pipeline.produced_dimension == envelope.dimension,
            "embedding envelope pipeline dimension mismatch"
        );
        let payload = &data[16 + envelope_len..];
        anyhow::ensure!(
            payload.len() as u64 == envelope.uid_table_bytes + envelope.vector_bytes,
            "embedding v2 payload length mismatch"
        );
        anyhow::ensure!(
            blake3::hash(payload).to_hex().as_str() == envelope.payload_blake3,
            "embedding v2 payload checksum mismatch"
        );
        let uid_end = envelope.uid_table_bytes as usize;
        let mut offset = 0usize;
        let mut uids = Vec::with_capacity(envelope.count as usize);
        while offset < uid_end {
            anyhow::ensure!(offset + 2 <= uid_end, "truncated embedding UID length");
            let length = u16::from_le_bytes(payload[offset..offset + 2].try_into()?) as usize;
            offset += 2;
            anyhow::ensure!(offset + length <= uid_end, "truncated embedding UID");
            uids.push(std::str::from_utf8(&payload[offset..offset + length])?.to_string());
            offset += length;
        }
        anyhow::ensure!(
            uids.len() == envelope.count as usize,
            "embedding UID count mismatch"
        );
        let dimension = envelope.dimension as usize;
        anyhow::ensure!(
            envelope.vector_bytes as usize == uids.len() * dimension * 4,
            "embedding vector byte count mismatch"
        );
        let vector_start = 16 + envelope_len + uid_end;
        let mut rows = Vec::with_capacity(uids.len());
        let mut row_by_uid = HashMap::with_capacity(uids.len());
        for (row, uid) in uids.into_iter().enumerate() {
            anyhow::ensure!(
                row_by_uid.insert(uid.clone(), row).is_none(),
                "duplicate embedding UID"
            );
            rows.push(EmbeddingBaseRow {
                uid,
                vector_offset: vector_start + row * dimension * 4,
            });
        }
        let fingerprint = envelope
            .pipeline
            .fingerprint()
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            embeddings: HashMap::new(),
            base: Some(EmbeddingBase {
                bytes: storage,
                rows,
                row_by_uid,
                dimension,
            }),
            deleted_base_uids: HashSet::new(),
            force_cleared: false,
            recorded_model_id: Some(envelope.pipeline.model_id.clone()),
            recorded_pipeline_fingerprint: Some(fingerprint),
            similarity: envelope.pipeline.similarity.clone(),
            artifact_envelope: Some(envelope),
            pending_deltas: Vec::new(),
            journal_sequence: 0,
            journal_valid_bytes: None,
        })
    }

    pub fn replay_journal_v2(
        &mut self,
        path: &Path,
        identity: &crate::PublicationIdentity,
        pipeline: &nestweaver_schema::EmbeddingPipelineV2,
    ) -> Result<(), anyhow::Error> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            bytes.len() >= EMBEDDING_JOURNAL_MAGIC.len()
                && &bytes[..EMBEDDING_JOURNAL_MAGIC.len()] == EMBEDDING_JOURNAL_MAGIC,
            "invalid embedding journal header"
        );
        let expected_pipeline = pipeline.fingerprint().map_err(anyhow::Error::msg)?;
        let mut offset = EMBEDDING_JOURNAL_MAGIC.len();
        let mut previous_sequence = 0u64;
        let mut valid_end = offset;
        while offset < bytes.len() {
            if offset + 4 > bytes.len() {
                break;
            }
            let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into()?) as usize;
            offset += 4;
            if offset + length > bytes.len() {
                break;
            }
            let record: EmbeddingJournalRecord =
                serde_json::from_slice(&bytes[offset..offset + length])?;
            offset += length;
            let canonical = serde_json::to_vec(&record.payload)?;
            anyhow::ensure!(
                blake3::hash(&canonical).to_hex().as_str() == record.checksum,
                "embedding journal record checksum mismatch"
            );
            anyhow::ensure!(
                record.payload.sequence == previous_sequence.saturating_add(1),
                "embedding journal sequence gap"
            );
            anyhow::ensure!(
                record.payload.brain_uuid == identity.brain_uuid
                    && record.payload.publication_uuid == identity.publication_uuid
                    && record.payload.pipeline_fingerprint == expected_pipeline,
                "embedding journal identity or pipeline mismatch"
            );
            match record.payload.delta {
                EmbeddingJournalDelta::Clear => {
                    self.embeddings.clear();
                    self.base = None;
                    self.deleted_base_uids.clear();
                }
                EmbeddingJournalDelta::Upsert { uid, vector } => {
                    anyhow::ensure!(
                        vector.len() == pipeline.produced_dimension as usize,
                        "embedding journal vector dimension mismatch"
                    );
                    self.deleted_base_uids.remove(&uid);
                    self.embeddings.insert(uid, vector);
                }
                EmbeddingJournalDelta::Delete { uid } => {
                    self.embeddings.remove(&uid);
                    if self.base.as_ref().is_some_and(|base| base.contains(&uid)) {
                        self.deleted_base_uids.insert(uid);
                    }
                }
            }
            previous_sequence = record.payload.sequence;
            valid_end = offset;
        }
        self.journal_sequence = previous_sequence;
        self.journal_valid_bytes = Some(valid_end as u64);
        self.pending_deltas.clear();
        Ok(())
    }

    pub fn append_journal_v2(
        &mut self,
        path: &Path,
        identity: &crate::PublicationIdentity,
        pipeline: &nestweaver_schema::EmbeddingPipelineV2,
    ) -> Result<usize, anyhow::Error> {
        if self.pending_deltas.is_empty() {
            return Ok(0);
        }
        let fingerprint = pipeline.fingerprint().map_err(anyhow::Error::msg)?;
        let mut journal = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                EMBEDDING_JOURNAL_MAGIC.to_vec()
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            journal.len() >= EMBEDDING_JOURNAL_MAGIC.len()
                && &journal[..EMBEDDING_JOURNAL_MAGIC.len()] == EMBEDDING_JOURNAL_MAGIC,
            "invalid embedding journal header"
        );
        if let Some(valid_bytes) = self.journal_valid_bytes {
            journal.truncate(valid_bytes as usize);
        }

        let count = self.pending_deltas.len();
        let mut next_sequence = self.journal_sequence;
        for delta in &self.pending_deltas {
            next_sequence = next_sequence.saturating_add(1);
            let delta = match delta {
                EmbeddingDelta::Clear => EmbeddingJournalDelta::Clear,
                EmbeddingDelta::Upsert { uid } => {
                    let vector = self.get(uid).ok_or_else(|| {
                        anyhow::anyhow!("pending embedding upsert {uid} has no vector")
                    })?;
                    EmbeddingJournalDelta::Upsert {
                        uid: uid.clone(),
                        vector,
                    }
                }
                EmbeddingDelta::Delete { uid } => {
                    EmbeddingJournalDelta::Delete { uid: uid.clone() }
                }
            };
            let payload = EmbeddingJournalPayload {
                sequence: next_sequence,
                brain_uuid: identity.brain_uuid.clone(),
                publication_uuid: identity.publication_uuid.clone(),
                pipeline_fingerprint: fingerprint.clone(),
                delta,
            };
            let checksum = blake3::hash(&serde_json::to_vec(&payload)?)
                .to_hex()
                .to_string();
            let encoded = serde_json::to_vec(&EmbeddingJournalRecord { payload, checksum })?;
            anyhow::ensure!(
                encoded.len() <= u32::MAX as usize,
                "embedding journal record too large"
            );
            journal.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            journal.extend_from_slice(&encoded);
        }

        // Keep the journal bounded and replace it atomically. A failed write
        // leaves both the prior journal and the in-memory pending delta set
        // intact, so the next checkpoint can retry without losing or
        // duplicating updates.
        atomic_replace_file(path, |file| std::io::Write::write_all(file, &journal))?;
        self.pending_deltas.clear();
        self.journal_sequence = next_sequence;
        self.journal_valid_bytes = None;
        Ok(count)
    }

    pub fn should_compact_journal(&self, path: &Path) -> bool {
        self.journal_sequence as usize >= EMBEDDING_JOURNAL_COMPACT_RECORDS
            || path
                .metadata()
                .is_ok_and(|metadata| metadata.len() >= EMBEDDING_JOURNAL_COMPACT_BYTES)
    }

    pub fn mark_base_persisted(&mut self) {
        self.pending_deltas.clear();
        self.journal_sequence = 0;
        self.journal_valid_bytes = None;
    }

    /// Reopen a newly compacted base as the live immutable mapping. This drops
    /// overlay vectors and tombstones only after the complete replacement has
    /// been validated, keeping steady-state heap usage proportional to UID
    /// metadata plus the bounded journal rather than corpus vector bytes.
    pub fn adopt_binary_v2(&mut self, path: &Path) -> Result<(), anyhow::Error> {
        let loaded = Self::load_binary_v2(path)?;
        self.embeddings = loaded.embeddings;
        self.base = loaded.base;
        self.deleted_base_uids = loaded.deleted_base_uids;
        self.recorded_model_id = loaded.recorded_model_id;
        self.recorded_pipeline_fingerprint = loaded.recorded_pipeline_fingerprint;
        self.similarity = loaded.similarity;
        self.artifact_envelope = loaded.artifact_envelope;
        self.force_cleared = false;
        self.mark_base_persisted();
        Ok(())
    }

    /// Return the top-`limit` (uid, similarity) pairs sorted descending.
    ///
    /// Uses rayon for parallel iteration and assumes stored embeddings are
    /// L2-normalized, so cosine similarity reduces to dot-product / query_norm.
    pub fn vector_search(&self, query_vec: &[f32], limit: usize) -> Vec<(String, f64)> {
        self.vector_search_cancellable(query_vec, limit, None)
            .expect("vector_search with cancel=None cannot be cancelled")
    }

    /// Like [`vector_search`], but cooperatively bails when `cancel` trips (a
    /// query timeout or client disconnect). Once tripped, per-embedding scoring
    /// is skipped so the parallel scan drains cheaply, then the whole call
    /// returns `Err(StoreError::Cancelled(_))` — a cancelled computation is
    /// *incomplete*, distinct from a legitimately empty result, so no caller
    /// mistakes the truncated scan for a real answer (or caches it).
    /// `cancel = None` never trips and is byte-for-byte the original behavior.
    pub fn vector_search_cancellable(
        &self,
        query_vec: &[f32],
        limit: usize,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let query_norm: f64 = query_vec
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            .sqrt();
        if query_norm == 0.0 {
            return Ok(vec![]);
        }

        if limit == 0 {
            return Ok(Vec::new());
        }

        let overlay_heap = self
            .embeddings
            .par_iter()
            .filter_map(|(uid, emb)| {
                if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                    return None;
                }
                // Exclude any stored vector whose dimension differs from the
                // query's. `.zip()` would otherwise truncate to the shorter and
                // return a plausible-but-wrong similarity (dot over a prefix,
                // divided by the full query norm) — silently corrupting rankings
                // when the index was built with a different embedding model.
                if emb.len() != query_vec.len() {
                    return None;
                }
                let sim = embedding_similarity(emb, query_vec, query_norm, &self.similarity);
                Some(RankedEmbedding {
                    uid: uid.clone(),
                    score: sim,
                })
            })
            .fold(BinaryHeap::new, |mut heap, candidate| {
                retain_top(&mut heap, candidate, limit);
                heap
            })
            .reduce(BinaryHeap::new, |left, right| merge_top(left, right, limit));

        let base_heap = self
            .base
            .as_ref()
            .filter(|base| base.dimension == query_vec.len())
            .map(|base| {
                base.rows
                    .par_iter()
                    .enumerate()
                    .filter_map(|(row_index, row)| {
                        if cancel
                            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
                            || self.deleted_base_uids.contains(&row.uid)
                            || self.embeddings.contains_key(&row.uid)
                        {
                            return None;
                        }
                        Some(RankedEmbedding {
                            uid: row.uid.clone(),
                            score: base.score_at(
                                row_index,
                                query_vec,
                                query_norm,
                                &self.similarity,
                            ),
                        })
                    })
                    .fold(BinaryHeap::new, |mut heap, candidate| {
                        retain_top(&mut heap, candidate, limit);
                        heap
                    })
                    .reduce(BinaryHeap::new, |left, right| merge_top(left, right, limit))
            })
            .unwrap_or_default();
        let heap = merge_top(overlay_heap, base_heap, limit);

        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
            // The shared cancel flag is a bare bool and can't carry a reason, so
            // the leaf always reports `Timeout` — the only reason the gRPC
            // boundary ever observes. (A client disconnect drops the request
            // future before any error is returned, so it never surfaces here.)
            return Err(StoreError::Cancelled(CancelReason::Timeout));
        }

        Ok(finish_top(heap))
    }

    pub fn len(&self) -> usize {
        let base_count = self.base.as_ref().map_or(0, |base| {
            base.rows
                .iter()
                .filter(|row| {
                    !self.deleted_base_uids.contains(&row.uid)
                        && !self.embeddings.contains_key(&row.uid)
                })
                .count()
        });
        base_count + self.embeddings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn has_pending_deltas(&self) -> bool {
        !self.pending_deltas.is_empty()
    }

    /// Return the dimensionality of the stored embeddings (length of the first
    /// vector found), or `None` if the index is empty.
    pub fn dimension(&self) -> Option<usize> {
        self.embeddings
            .values()
            .next()
            .map(Vec::len)
            .or_else(|| self.base.as_ref().map(|base| base.dimension))
    }

    /// Like `vector_search`, but pre-filters embeddings whose UID contains `uid_prefix`.
    /// When `uid_prefix` is `None`, behaves identically to `vector_search`.
    pub fn vector_search_filtered(
        &self,
        query_vec: &[f32],
        limit: usize,
        uid_prefix: Option<&str>,
    ) -> Vec<(String, f64)> {
        let query_norm: f64 = query_vec
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            .sqrt();
        if query_norm == 0.0 {
            return vec![];
        }

        let overlay_heap = self
            .embeddings
            .par_iter()
            .filter(|(uid, _)| match uid_prefix {
                Some(prefix) => uid.contains(prefix),
                None => true,
            })
            .filter_map(|(uid, emb)| {
                // See vector_search_cancellable: a dimension mismatch must be
                // excluded, not silently truncated by `.zip()`.
                if emb.len() != query_vec.len() {
                    return None;
                }
                let sim = embedding_similarity(emb, query_vec, query_norm, &self.similarity);
                Some(RankedEmbedding {
                    uid: uid.clone(),
                    score: sim,
                })
            })
            .fold(BinaryHeap::new, |mut heap, candidate| {
                retain_top(&mut heap, candidate, limit);
                heap
            })
            .reduce(BinaryHeap::new, |left, right| merge_top(left, right, limit));

        let base_heap = self
            .base
            .as_ref()
            .filter(|base| base.dimension == query_vec.len())
            .map(|base| {
                base.rows
                    .par_iter()
                    .enumerate()
                    .filter_map(|(row_index, row)| {
                        if self.deleted_base_uids.contains(&row.uid)
                            || self.embeddings.contains_key(&row.uid)
                            || uid_prefix.is_some_and(|prefix| !row.uid.contains(prefix))
                        {
                            return None;
                        }
                        Some(RankedEmbedding {
                            uid: row.uid.clone(),
                            score: base.score_at(
                                row_index,
                                query_vec,
                                query_norm,
                                &self.similarity,
                            ),
                        })
                    })
                    .fold(BinaryHeap::new, |mut heap, candidate| {
                        retain_top(&mut heap, candidate, limit);
                        heap
                    })
                    .reduce(BinaryHeap::new, |left, right| merge_top(left, right, limit))
            })
            .unwrap_or_default();

        finish_top(merge_top(overlay_heap, base_heap, limit))
    }

    /// Look up the embedding for a given UID.
    pub fn get(&self, uid: &str) -> Option<Vec<f32>> {
        if let Some(vector) = self.embeddings.get(uid) {
            return Some(vector.clone());
        }
        if self.deleted_base_uids.contains(uid) {
            return None;
        }
        let base = self.base.as_ref()?;
        base.row_by_uid.get(uid).map(|row| base.vector_at(*row))
    }

    fn all_uids(&self) -> Vec<String> {
        let mut uids: HashSet<String> = self.embeddings.keys().cloned().collect();
        if let Some(base) = &self.base {
            uids.extend(
                base.rows
                    .iter()
                    .filter(|row| !self.deleted_base_uids.contains(&row.uid))
                    .map(|row| row.uid.clone()),
            );
        }
        uids.into_iter().collect()
    }

    fn snapshot_entries(&self) -> Vec<EmbeddingSnapshotEntry<'_>> {
        let mut entries = self
            .embeddings
            .iter()
            .map(|(uid, vector)| EmbeddingSnapshotEntry {
                uid,
                vector: EmbeddingVectorRef::Overlay(vector),
            })
            .collect::<Vec<_>>();
        if let Some(base) = &self.base {
            entries.extend(base.rows.iter().enumerate().filter_map(|(row_index, row)| {
                if self.deleted_base_uids.contains(&row.uid)
                    || self.embeddings.contains_key(&row.uid)
                {
                    return None;
                }
                Some(EmbeddingSnapshotEntry {
                    uid: &row.uid,
                    vector: EmbeddingVectorRef::Base(base.vector_bytes_at(row_index)),
                })
            }));
        }
        entries.sort_by(|left, right| left.uid.cmp(right.uid));
        entries
    }

    /// Retain only embeddings whose graph nodes still exist.
    ///
    /// Returns the number of removed vectors so callers can report and test
    /// reconciliation without exposing the index's internal map.
    pub(crate) fn retain_uids(&mut self, live_uids: &std::collections::HashSet<String>) -> usize {
        let before = self.len();
        let removed: Vec<String> = self
            .all_uids()
            .into_iter()
            .filter(|uid| !live_uids.contains(uid.as_str()))
            .collect();
        for uid in removed {
            self.embeddings.remove(&uid);
            if self.base.as_ref().is_some_and(|base| base.contains(&uid)) {
                self.deleted_base_uids.insert(uid.clone());
            }
            self.pending_deltas.push(EmbeddingDelta::Delete { uid });
        }
        before - self.len()
    }
}

fn atomic_replace_file(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), anyhow::Error> {
    crate::durable_sidecar::atomic_replace_file(path, write).map_err(Into::into)
}

/// Default cadence for time-based embedding-index checkpoints during long
/// embed passes (CLI direct loops and the daemon embed RPC).
pub const EMBED_CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Time-based checkpoint for long embed passes.
///
/// An interrupted embed pass used to lose everything: the only flush ran
/// once at the end. `flush_if_due` checkpoints the bounded delta journal at
/// chunk boundaries once `interval` has elapsed and new embeddings were
/// accepted since the last checkpoint, so a killed pass keeps all work up to
/// the last durable boundary without rewriting the mmap base each batch.
///
/// Mid-run call sites (CLI and daemon embed loops) should use
/// [`Self::flush_if_due_with_stamp`] so the model fingerprint travels with
/// each checkpoint; plain `flush_if_due` remains for the final flush, which
/// is followed by the run's tail stamp.
pub struct EmbeddingFlushCheckpoint {
    interval: std::time::Duration,
    last_flush: std::time::Instant,
    flushed_success_count: usize,
}

impl EmbeddingFlushCheckpoint {
    pub fn new(interval: std::time::Duration) -> Self {
        Self {
            interval,
            last_flush: std::time::Instant::now(),
            flushed_success_count: 0,
        }
    }

    /// Flush the embedding index when `interval` elapsed and `success_count`
    /// advanced since the last flush. Returns whether a flush happened.
    pub fn flush_if_due(
        &mut self,
        store: &GraphStore,
        success_count: usize,
    ) -> Result<bool, StoreError> {
        if !self.is_due(success_count) {
            return Ok(false);
        }
        store.flush_embedding_index()?;
        self.record_flush(success_count);
        Ok(true)
    }

    /// Checkpoint like [`Self::flush_if_due`], then stamp the flushed index's
    /// fingerprint (`model_id`, `produced_dim`) into the embedding metadata.
    ///
    /// The fingerprint must travel with the checkpoint: a `--force` run
    /// switching to a different model at the same dimension overwrites old
    /// vectors as it goes, so a checkpoint that persisted vectors without
    /// updating the metadata would leave a durable mixed-model index whose
    /// metadata still names the OLD model — every open-time guard would pass
    /// and a later non-force embed would see all-present embeddings and do
    /// zero work. Stamping here means an interrupted force-switch instead
    /// leaves a consistent (new-model, partial) index that the next run
    /// resumes or re-embeds honestly.
    ///
    /// The stamp keys off `produced_dim` (the dimension of vectors THIS run
    /// produced): when it is `None` the run produced nothing yet and the
    /// existing fingerprint is left untouched. Metadata is persisted before
    /// the vector checkpoint so readers never accept vectors under stale
    /// producer metadata. Either failure prevents this checkpoint from
    /// advancing.
    pub fn flush_if_due_with_stamp(
        &mut self,
        store: &GraphStore,
        success_count: usize,
        model_id: &str,
        produced_dim: Option<usize>,
    ) -> Result<bool, StoreError> {
        if !self.is_due(success_count) {
            return Ok(false);
        }
        if let Some(dim) = produced_dim
            && !model_id.is_empty()
        {
            store.set_embedding_metadata(model_id, dim as u32)?;
        }
        store.flush_embedding_index()?;
        self.record_flush(success_count);
        Ok(true)
    }

    pub fn flush_if_due_with_pipeline(
        &mut self,
        store: &GraphStore,
        success_count: usize,
        pipeline: Option<&nestweaver_schema::EmbeddingPipelineV2>,
    ) -> Result<bool, StoreError> {
        if !self.is_due(success_count) {
            return Ok(false);
        }
        if let Some(pipeline) = pipeline {
            store.set_embedding_pipeline(pipeline)?;
        }
        store.flush_embedding_index()?;
        self.record_flush(success_count);
        Ok(true)
    }

    fn is_due(&self, success_count: usize) -> bool {
        success_count != self.flushed_success_count && self.last_flush.elapsed() >= self.interval
    }

    fn record_flush(&mut self, success_count: usize) {
        self.last_flush = std::time::Instant::now();
        self.flushed_success_count = success_count;
    }
}

#[cfg(test)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let norm_a: f64 = a
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
    pub signature: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Hybrid search on GraphStore
// ---------------------------------------------------------------------------

impl GraphStore {
    /// Hybrid search combining text (substring) and optional vector similarity via
    /// Reciprocal Rank Fusion (RRF).
    ///
    /// * `text_query`       – substring passed to `search_symbols_by_name`
    /// * `query_embedding`  – embedding of the query (optional)
    /// * `embedding_index`  – pre-loaded `EmbeddingIndex` (optional)
    /// * `limit`            – maximum results to return
    /// * `seed_resolution`  – path-deboost + kind-priority for seed scoring
    pub fn hybrid_search(
        &self,
        text_query: &str,
        query_embedding: Option<&[f32]>,
        embedding_index: Option<&EmbeddingIndex>,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
    ) -> Result<Vec<SearchResult>, StoreError> {
        self.hybrid_search_cancellable(
            text_query,
            query_embedding,
            embedding_index,
            limit,
            seed_resolution,
            None,
        )
    }

    /// Like [`hybrid_search`], but threads a cooperative cancellation flag into
    /// the parallel vector scan. `cancel = None` is the original behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_cancellable(
        &self,
        text_query: &str,
        query_embedding: Option<&[f32]>,
        embedding_index: Option<&EmbeddingIndex>,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<SearchResult>, StoreError> {
        // 1. Text search
        let text_results = self.search_symbols_by_name(text_query, limit * 2, seed_resolution)?;

        // 2. Vector search (only when both embedding and index are present)
        let vec_results: Vec<(String, f64)> = match (query_embedding, embedding_index) {
            (Some(qe), Some(idx)) => idx.vector_search_cancellable(qe, limit * 2, cancel)?,
            _ => vec![],
        };

        // 3. Reciprocal Rank Fusion
        let k = 60.0_f64;
        let mut rrf_scores: HashMap<String, f64> = HashMap::new();

        for (rank, sym) in text_results.iter().enumerate() {
            *rrf_scores.entry(sym.uid.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, (uid, _)) in vec_results.iter().enumerate() {
            *rrf_scores.entry(uid.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // 4. Sort by RRF score descending
        let mut merged: Vec<(String, f64)> = rrf_scores.into_iter().collect();
        merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(limit);

        // 5. Map UIDs back to SearchResult, preferring the already-fetched text_results
        let results = merged
            .into_iter()
            .filter_map(|(uid, score)| {
                // Try text_results first (cheap path)
                let found = text_results.iter().find(|s| s.uid == uid);
                if let Some(s) = found {
                    return Some(SearchResult {
                        uid: s.uid.clone(),
                        name: s.name.clone(),
                        kind: s.kind.to_string(),
                        file_path: s.file_path.clone(),
                        start_line: s.start_line,
                        signature: s.signature.clone(),
                        score,
                    });
                }
                // Fall back to a point lookup for vector-only hits
                self.lookup_symbol(&uid).ok().map(|s| SearchResult {
                    uid: s.uid,
                    name: s.name,
                    kind: s.kind.to_string(),
                    file_path: s.file_path,
                    start_line: s.start_line,
                    signature: s.signature,
                    score,
                })
            })
            .collect();

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{EmbeddingPipelineV2, Symbol, SymbolKind, Visibility};
    use std::io::Write as _;

    fn test_pipeline(model: &str, dimension: u32) -> EmbeddingPipelineV2 {
        EmbeddingPipelineV2::external("test-provider", model, dimension)
    }

    fn test_identity() -> crate::PublicationIdentity {
        crate::PublicationIdentity {
            brain_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
            publication_uuid: "00000000-0000-4000-8000-000000000002".to_string(),
        }
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let s = cosine_similarity(&a, &a);
        assert!((s - 1.0).abs() < 1e-6, "expected 1.0, got {s}");
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 1e-6, "expected 0.0, got {s}");
    }

    #[test]
    fn cosine_similarity_empty_returns_zero() {
        let s = cosine_similarity(&[], &[]);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn add_rejects_dimension_mismatched_vector() {
        // The index must stay homogeneous so the binary sidecar can't misalign.
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:a", vec![1.0_f32, 0.0, 0.0], false)); // establishes dim 3
        assert!(!idx.add("sym:b", vec![1.0_f32, 0.0], false)); // dim 2 — must be rejected
        assert_eq!(idx.len(), 1, "mismatched-dim vector must not be added");
        assert_eq!(idx.dimension(), Some(3));
    }

    #[test]
    fn add_force_clears_index_on_dimension_change() {
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:a", vec![1.0_f32, 0.0, 0.0], false));
        assert!(idx.add("sym:b", vec![0.0_f32, 1.0, 0.0], false));
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.dimension(), Some(3));

        // Force with new dimension clears existing entries
        assert!(idx.add("sym:c", vec![1.0_f32, 0.0], true));
        assert_eq!(idx.len(), 1, "force should clear old entries");
        assert_eq!(
            idx.dimension(),
            Some(2),
            "dimension should switch to new model's"
        );

        // Subsequent adds with matching dimension work normally
        assert!(idx.add("sym:d", vec![0.0_f32, 1.0], false));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn add_force_clears_at_most_once_per_run() {
        // A mixed-dimension source under --force must not ping-pong-wipe the
        // index: the first flip is a legitimate model switch, a second flip in
        // the same run means the source is emitting mixed dimensions and its
        // vectors must be rejected, not honored with another clear.
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:a", vec![1.0_f32, 0.0, 0.0], false)); // dim 3
        assert!(idx.add("sym:b", vec![1.0_f32, 0.0], true)); // dim 2: first clear
        assert!(idx.add("sym:c", vec![0.0_f32, 1.0], true)); // dim 2: normal add
        assert_eq!(idx.len(), 2);

        // dim-3 straggler (e.g. a mid-run fallback model) — rejected, no wipe
        assert!(!idx.add("sym:d", vec![1.0_f32, 0.0, 0.0], true));
        assert_eq!(idx.len(), 2, "second flip must not clear again");
        assert_eq!(idx.dimension(), Some(2));
    }

    #[test]
    fn reset_force_guard_allows_a_later_model_switch() {
        // The daemon's index outlives embed runs; a fresh run re-arms the
        // guard so a second deliberate model switch works.
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:a", vec![1.0_f32, 0.0, 0.0], false)); // dim 3
        assert!(idx.add("sym:b", vec![1.0_f32, 0.0], true)); // switch to dim 2

        idx.reset_force_guard();
        assert!(idx.add("sym:c", vec![1.0_f32, 0.0, 0.0], true)); // switch back to dim 3
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.dimension(), Some(3));
    }

    // ── Recorded-model write guard ────────────────────────────────────
    // A same-dimension model swap passes the dimension guard, but two
    // contrastively trained embedding spaces share no basis, so a
    // mixed-model index is unusable for retrieval. `add_with_model` refuses
    // the write (naming both ids via tracing) unless the run is `--force`.

    #[test]
    fn add_with_model_rejects_a_different_recorded_model_at_the_same_dimension() {
        let mut idx = EmbeddingIndex::new();
        idx.set_recorded_model_id(Some("sentence-transformers/model-a".to_string()));
        assert!(idx.add_with_model(
            "sym:a",
            vec![1.0_f32, 0.0, 0.0],
            Some("sentence-transformers/model-a"),
            false,
        ));

        // Same dimension, different producer — the hole the dimension guard
        // cannot see. Must be rejected, and the index must stay untouched.
        assert!(!idx.add_with_model(
            "sym:b",
            vec![0.0_f32, 1.0, 0.0],
            Some("sentence-transformers/model-b"),
            false,
        ));
        assert_eq!(idx.len(), 1, "a cross-model vector must not be added");
        assert_eq!(idx.dimension(), Some(3));
        assert!(idx.get("sym:b").is_none());
    }

    #[test]
    fn add_with_model_accepts_the_recorded_model() {
        let mut idx = EmbeddingIndex::new();
        idx.set_recorded_model_id(Some("sentence-transformers/model-a".to_string()));
        assert!(idx.add_with_model(
            "sym:a",
            vec![1.0_f32, 0.0, 0.0],
            Some("sentence-transformers/model-a"),
            false,
        ));
        assert!(idx.add_with_model(
            "sym:b",
            vec![0.0_f32, 1.0, 0.0],
            Some("sentence-transformers/model-a"),
            false,
        ));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn add_with_model_allows_any_model_when_none_is_recorded() {
        // Absent metadata means unknown, not mismatch: a database that was
        // never stamped must keep accepting writes (guarded by dimension
        // only), or every pre-fingerprint database would be frozen.
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add_with_model(
            "sym:a",
            vec![1.0_f32, 0.0, 0.0],
            Some("sentence-transformers/model-a"),
            false,
        ));
        assert!(idx.add_with_model(
            "sym:b",
            vec![0.0_f32, 1.0, 0.0],
            Some("sentence-transformers/model-b"),
            false,
        ));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn add_with_model_force_overrides_the_recorded_model_guard() {
        // A --force run re-embeds everything, so no mixture survives; the
        // switch must be allowed through.
        let mut idx = EmbeddingIndex::new();
        idx.set_recorded_model_id(Some("sentence-transformers/model-a".to_string()));
        assert!(idx.add_with_model(
            "sym:a",
            vec![1.0_f32, 0.0, 0.0],
            Some("sentence-transformers/model-a"),
            false,
        ));
        assert!(idx.add_with_model(
            "sym:b",
            vec![0.0_f32, 1.0, 0.0],
            Some("sentence-transformers/model-b"),
            true,
        ));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn add_without_a_producer_model_skips_the_model_guard() {
        // `add` names no producing model (unknown producer), so the model
        // guard cannot fire on it; the dimension guard still applies.
        let mut idx = EmbeddingIndex::new();
        idx.set_recorded_model_id(Some("sentence-transformers/model-a".to_string()));
        assert!(idx.add("sym:a", vec![1.0_f32, 0.0, 0.0], false));
        assert!(idx.add("sym:b", vec![0.0_f32, 1.0, 0.0], false));
        assert!(!idx.add("sym:c", vec![1.0_f32, 0.0], false));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn store_model_guard_reads_the_recorded_model_from_a_reopened_database() {
        // The recorded model id is persisted in the database's embedding
        // metadata and handed to the index once at open — a store that
        // reopens a stamped database must enforce the guard without anyone
        // re-stamping.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("model_guard.lbug");
        {
            let store = GraphStore::create(&db_path).unwrap();
            store
                .set_embedding_metadata("sentence-transformers/model-a", 3)
                .unwrap();
            assert!(store.add_embedding_with_force(
                "sym:a",
                vec![1.0_f32, 0.0, 0.0],
                "sentence-transformers/model-a",
                false,
            ));
            store.flush_embedding_index().unwrap();
        }

        let store = GraphStore::open(&db_path).unwrap();
        assert!(
            !store.add_embedding_with_force(
                "sym:b",
                vec![0.0_f32, 1.0, 0.0],
                "sentence-transformers/model-b",
                false,
            ),
            "a same-dimension write from a different model must be rejected"
        );
        assert!(!store.has_embedding("sym:b"));
        assert!(store.add_embedding_with_force(
            "sym:c",
            vec![0.0_f32, 1.0, 0.0],
            "sentence-transformers/model-a",
            false,
        ));
        assert!(store.add_embedding_with_force(
            "sym:d",
            vec![1.0_f32, 1.0, 0.0],
            "sentence-transformers/model-b",
            true,
        ));
    }

    #[test]
    fn set_embedding_metadata_refreshes_the_in_memory_recorded_model() {
        // Long-lived stores (the daemon) must check new writes against the
        // fingerprint that was just stamped, not the one read at open.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("model_refresh.lbug")).unwrap();
        store
            .set_embedding_metadata("sentence-transformers/model-a", 3)
            .unwrap();
        assert!(!store.add_embedding_with_force(
            "sym:a",
            vec![1.0_f32, 0.0, 0.0],
            "sentence-transformers/model-b",
            false,
        ));

        store
            .set_embedding_metadata("sentence-transformers/model-b", 3)
            .unwrap();
        assert!(
            store.add_embedding_with_force(
                "sym:a",
                vec![1.0_f32, 0.0, 0.0],
                "sentence-transformers/model-b",
                false,
            ),
            "writes must be checked against the newly stamped model"
        );
        assert!(!store.add_embedding_with_force(
            "sym:b",
            vec![0.0_f32, 1.0, 0.0],
            "sentence-transformers/model-a",
            false,
        ));
    }

    // ── Embedding flush checkpoint ─────────────────────────────────────
    // An interrupted embed pass used to lose everything: the only flush ran
    // once at the end. The checkpoint flushes at chunk boundaries once the
    // interval elapsed AND new embeddings were accepted since the last
    // flush. The 300s cadence itself lives in `EMBED_CHECKPOINT_INTERVAL`;
    // these tests drive the journal checkpoint helper with a zero/long
    // interval instead.

    #[test]
    fn flush_if_due_does_not_flush_before_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        assert!(store.add_embedding("sym:a", vec![0.1_f32, 0.2, 0.3]));
        let sidecar = store.embedding_sidecar_path().unwrap();

        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::from_secs(3600));
        assert!(
            !checkpoint.flush_if_due(&store, 1).unwrap(),
            "no flush may happen before the interval elapses"
        );
        assert!(!sidecar.exists(), "nothing may be written early");
    }

    #[test]
    fn flush_if_due_without_new_work_never_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        let sidecar = store.embedding_sidecar_path().unwrap();

        // A zero interval means always due; with no accepted embeddings the
        // checkpoint must still not write (there is nothing new to persist).
        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::ZERO);
        assert!(!checkpoint.flush_if_due(&store, 0).unwrap());
        assert!(!sidecar.exists());
    }

    #[test]
    fn flush_if_due_flushes_when_due_with_new_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        let sidecar = store.embedding_sidecar_path().unwrap();
        store.set_embedding_metadata("test-model", 3).unwrap();
        assert!(store.add_embedding("sym:a", vec![0.1_f32, 0.2, 0.3]));

        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::ZERO);
        assert!(
            checkpoint.flush_if_due(&store, 1).unwrap(),
            "a due checkpoint with newly accepted embeddings must flush"
        );
        assert!(sidecar.exists(), "the flush must persist the sidecar");

        // A due checkpoint with no new accepts since the last flush must not
        // rewrite the sidecar (each rewrite rewrites every vector).
        assert!(!checkpoint.flush_if_due(&store, 1).unwrap());

        // Newly accepted work since the flush re-arms it.
        assert!(store.add_embedding("sym:b", vec![0.4_f32, 0.5, 0.6]));
        assert!(checkpoint.flush_if_due(&store, 2).unwrap());
    }

    #[test]
    fn flush_if_due_with_stamp_stamps_the_fingerprint_with_the_checkpoint() {
        // A due checkpoint persists vectors AND the fingerprint describing
        // them, so an interrupted --force model switch leaves a consistent
        // (new-model, partial) index instead of mixed vectors under the old
        // model's metadata.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        let sidecar = store.embedding_sidecar_path().unwrap();
        assert!(store.add_embedding_with_force(
            "sym:a",
            vec![0.1_f32, 0.2, 0.3],
            "sentence-transformers/model-b",
            true,
        ));

        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::ZERO);
        assert!(
            checkpoint
                .flush_if_due_with_stamp(&store, 1, "sentence-transformers/model-b", Some(3))
                .unwrap(),
            "a due checkpoint with newly accepted embeddings must flush"
        );
        assert!(sidecar.exists(), "the flush must persist the sidecar");
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some(("sentence-transformers/model-b".to_string(), 3)),
            "the checkpoint must stamp the fingerprint of the vectors it persisted"
        );
        // The in-memory recorded model refreshes too, so subsequent writes in
        // the same run are guarded against the newly stamped model.
        assert!(store.add_embedding_with_force(
            "sym:b",
            vec![0.4_f32, 0.5, 0.6],
            "sentence-transformers/model-b",
            false,
        ));
    }

    #[test]
    fn flush_if_due_with_stamp_without_new_work_neither_flushes_nor_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        let sidecar = store.embedding_sidecar_path().unwrap();

        // A zero interval means always due; with no accepted embeddings the
        // checkpoint must still not write or stamp (there is nothing new to
        // persist).
        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::ZERO);
        assert!(
            !checkpoint
                .flush_if_due_with_stamp(&store, 0, "sentence-transformers/model-b", Some(3))
                .unwrap()
        );
        assert!(!sidecar.exists());
        assert_eq!(store.get_embedding_metadata().unwrap(), None);
    }

    #[test]
    fn flush_if_due_with_stamp_with_no_produced_dim_flushes_but_does_not_stamp() {
        // `produced_dim == None` means this run produced nothing, so the
        // pre-existing fingerprint must survive even when a flush fires.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("ckpt.lbug")).unwrap();
        store
            .set_embedding_metadata("sentence-transformers/model-a", 3)
            .unwrap();
        assert!(store.add_embedding("sym:a", vec![0.1_f32, 0.2, 0.3]));

        let mut checkpoint = EmbeddingFlushCheckpoint::new(std::time::Duration::ZERO);
        assert!(
            checkpoint
                .flush_if_due_with_stamp(&store, 1, "sentence-transformers/model-b", None)
                .unwrap(),
            "the flush decision keys off new accepts, not produced_dim"
        );
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some(("sentence-transformers/model-a".to_string(), 3)),
            "a checkpoint with no produced vectors must not overwrite the fingerprint"
        );
    }

    #[test]
    fn vector_search_excludes_dimension_mismatched_vectors() {
        // Defense-in-depth for the query path: simulate a legacy/loaded index that
        // somehow holds a mismatched vector (insert directly, bypassing the add
        // guard). Before the query guard, `.zip()` truncated and returned a
        // plausible-but-wrong score.
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:right", vec![1.0_f32, 0.0, 0.0], false));
        idx.embeddings
            .insert("sym:wrongdim".to_string(), vec![1.0_f32, 0.0]);
        let query = vec![1.0_f32, 0.0, 0.0];
        let results = idx.vector_search(&query, 10);
        // The matching-dim vector scores ~1.0; the mismatched one is absent.
        let right = results.iter().find(|(u, _)| u == "sym:right").unwrap();
        assert!((right.1 - 1.0).abs() < 1e-6, "got {}", right.1);
        assert!(
            !results.iter().any(|(uid, _)| uid == "sym:wrongdim"),
            "mismatched-dim vector must not be returned: {results:?}"
        );
    }

    #[test]
    fn filtered_vector_search_excludes_dimension_mismatched_vectors() {
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:right", vec![1.0_f32, 0.0, 0.0], false));
        idx.embeddings
            .insert("sym:wrongdim".to_string(), vec![1.0_f32, 0.0]);

        let results = idx.vector_search_filtered(&[1.0, 0.0, 0.0], 10, Some("sym:"));
        assert_eq!(results.len(), 1, "only matching dimensions may be scored");
        assert_eq!(results[0].0, "sym:right");
    }

    #[test]
    fn cosine_similarity_mismatched_len_returns_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn vector_search_cancellable_uncancelled_returns_results() {
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("a", vec![1.0, 0.0, 0.0], false));
        assert!(idx.add("b", vec![0.9, 0.1, 0.0], false));
        assert!(idx.add("c", vec![0.0, 0.0, 1.0], false));

        // Not cancelled → normal results.
        let live = idx
            .vector_search_cancellable(&[1.0, 0.0, 0.0], 3, None)
            .expect("an uncancelled search cannot be cancelled");
        assert!(
            !live.is_empty(),
            "an uncancelled vector search returns results"
        );
    }

    #[test]
    fn vector_search_cancellable_returns_err_not_empty_on_cancel() {
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("a", vec![1.0, 0.0, 0.0], false));
        assert!(idx.add("b", vec![0.9, 0.1, 0.0], false));
        assert!(idx.add("c", vec![0.0, 0.0, 1.0], false));

        // Pre-cancelled over a NON-empty candidate set: a cancelled computation
        // is incomplete, not empty — it must surface as a distinct error so no
        // caller mistakes the truncated scan for a legitimate empty result.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let res = idx.vector_search_cancellable(&[1.0, 0.0, 0.0], 3, Some(&cancel));
        assert!(
            matches!(res, Err(StoreError::Cancelled(_))),
            "a cancelled vector search must return Err(Cancelled), got {res:?}"
        );
    }

    #[test]
    fn vector_search_returns_most_similar() {
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("a", vec![1.0, 0.0, 0.0], false));
        assert!(idx.add("b", vec![0.9, 0.1, 0.0], false));
        assert!(idx.add("c", vec![0.0, 0.0, 1.0], false));

        let results = idx.vector_search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[1].0, "b");
    }

    #[test]
    fn vector_search_limit_respected() {
        let mut idx = EmbeddingIndex::new();
        for i in 0..10 {
            assert!(idx.add(&format!("sym:{i}"), vec![i as f32, 0.0], false));
        }
        let results = idx.vector_search(&[1.0, 0.0], 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn embedding_index_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.json");
        let mut idx = EmbeddingIndex::new();
        // Use an L2-normalized vector (vector_search assumes pre-normalized embeddings)
        let norm = (0.1_f32 * 0.1 + 0.2 * 0.2 + 0.3 * 0.3).sqrt();
        let v = vec![0.1 / norm, 0.2 / norm, 0.3 / norm];
        assert!(idx.add("sym:test", v.clone(), false));
        idx.save(&path).unwrap();

        let loaded = EmbeddingIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let results = loaded.vector_search(&v, 1);
        assert_eq!(results[0].0, "sym:test");
        assert!((results[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn embedding_index_is_empty() {
        let idx = EmbeddingIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    fn make_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "r".to_string(),
            file_path: "a.js".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("function {name}()"),
            summary: None,
            content_hash: "x".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn empty_seed_resolution() -> SeedResolutionConfig {
        SeedResolutionConfig {
            path_deboost: Vec::new(),
            kind_priority: Vec::new(),
        }
    }

    #[test]
    fn hybrid_search_text_only() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("sym:1", "greet")).unwrap();

        let results = store
            .hybrid_search("greet", None, None, 10, &empty_seed_resolution())
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "greet");
    }

    #[test]
    fn hybrid_search_no_match_returns_empty() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("sym:1", "greet")).unwrap();

        let results = store
            .hybrid_search("zzznomatch", None, None, 10, &empty_seed_resolution())
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn binary_save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.bin");

        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:alpha", vec![0.1, 0.2, 0.3], false));
        assert!(idx.add("sym:beta", vec![0.4, 0.5, 0.6], false));
        assert!(idx.add("sym:gamma", vec![0.7, 0.8, 0.9], false));
        idx.save_binary(&path).unwrap();

        let loaded = EmbeddingIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        // Verify each vector survived the round-trip
        for uid in &["sym:alpha", "sym:beta", "sym:gamma"] {
            let orig = idx.get(uid).unwrap();
            let rt = loaded.get(uid).unwrap();
            assert_eq!(orig, rt, "round-trip mismatch for {uid}");
        }
    }

    #[test]
    fn binary_v2_round_trip_binds_identity_pipeline_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings-v2.bin");
        let identity = test_identity();
        let pipeline = test_pipeline("model-a", 3);
        let mut index = EmbeddingIndex::new();
        assert!(index.add_with_pipeline("sym:beta", vec![0.4, 0.5, 0.6], &pipeline, false));
        assert!(index.add_with_pipeline("sym:alpha", vec![0.1, 0.2, 0.3], &pipeline, false));

        let envelope = index
            .save_binary_v2(&path, &identity, 42, &pipeline)
            .unwrap();
        assert_eq!(envelope.brain_uuid, identity.brain_uuid);
        assert_eq!(envelope.publication_uuid, identity.publication_uuid);
        assert_eq!(envelope.source_graph_generation, 42);
        assert_eq!(envelope.pipeline, pipeline);
        assert_eq!(envelope.count, 2);

        let loaded = EmbeddingIndex::load_binary_v2(&path).unwrap();
        assert!(
            matches!(
                loaded.base.as_ref().map(|base| &base.bytes),
                Some(EmbeddingBaseBytes::Mapped(_))
            ),
            "file-backed v2 loads must memory-map the immutable base"
        );
        assert!(
            loaded.embeddings.is_empty(),
            "base vectors must not be heap-copied"
        );
        assert_eq!(loaded.get("sym:alpha"), Some(vec![0.1, 0.2, 0.3]));
        assert_eq!(loaded.get("sym:beta"), Some(vec![0.4, 0.5, 0.6]));

        let mut corrupt = std::fs::read(&path).unwrap();
        *corrupt.last_mut().unwrap() ^= 0xff;
        let error = match EmbeddingIndex::load_binary_v2_bytes(&corrupt) {
            Ok(_) => panic!("corrupt embedding payload must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn binary_v2_compaction_merges_mapped_base_overlay_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let compacted = dir.path().join("compacted.bin");
        let identity = test_identity();
        let pipeline = test_pipeline("model-a", 2);
        let mut original = EmbeddingIndex::new();
        for (uid, vector) in [
            ("a", vec![1.0, 0.0]),
            ("b", vec![0.0, 1.0]),
            ("c", vec![0.5, 0.5]),
        ] {
            assert!(original.add_with_pipeline(uid, vector, &pipeline, false));
        }
        original
            .save_binary_v2(&first, &identity, 1, &pipeline)
            .unwrap();

        let mut reopened = EmbeddingIndex::load_binary_v2(&first).unwrap();
        assert!(reopened.add_with_pipeline("b", vec![0.25, 0.75], &pipeline, false));
        assert!(reopened.add_with_pipeline("d", vec![0.75, 0.25], &pipeline, false));
        let live = HashSet::from(["b".to_string(), "c".to_string(), "d".to_string()]);
        assert_eq!(reopened.retain_uids(&live), 1);
        let envelope = reopened
            .save_binary_v2(&compacted, &identity, 2, &pipeline)
            .unwrap();
        assert_eq!(envelope.count, 3);

        let compacted = EmbeddingIndex::load_binary_v2(&compacted).unwrap();
        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted.get("b"), Some(vec![0.25, 0.75]));
        assert_eq!(compacted.get("c"), Some(vec![0.5, 0.5]));
        assert_eq!(compacted.get("d"), Some(vec![0.75, 0.25]));
        assert!(compacted.get("a").is_none());
    }

    #[test]
    fn pipeline_guard_rejects_same_dimension_semantic_mixing() {
        let first = test_pipeline("model-a", 3);
        let second = test_pipeline("model-b", 3);
        let third = test_pipeline("model-c", 3);
        let mut index = EmbeddingIndex::new();
        assert!(index.add_with_pipeline("a", vec![1.0, 0.0, 0.0], &first, false));
        assert!(!index.add_with_pipeline("b", vec![0.0, 1.0, 0.0], &second, false));
        assert_eq!(index.len(), 1);

        assert!(index.add_with_pipeline("b", vec![0.0, 1.0, 0.0], &second, true));
        assert_eq!(index.len(), 1, "forced switch must clear the old space");
        assert!(index.get("a").is_none());
        assert!(
            !index.add_with_pipeline("c", vec![0.0, 0.0, 1.0], &third, true),
            "a second semantic-space switch in one run must be refused"
        );
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn empty_index_allows_first_vector_to_replace_intended_pipeline() {
        let intended = test_pipeline("model-a", 768);
        let produced = test_pipeline("model-a", 3);
        let mut index = EmbeddingIndex::new();
        index.set_recorded_model_id(Some(intended.model_id.clone()));
        index.set_recorded_pipeline_fingerprint(Some(intended.fingerprint().unwrap()));

        assert!(index.add_with_pipeline("first", vec![1.0, 0.0, 0.0], &produced, false));
        assert_eq!(index.len(), 1);
        assert_eq!(
            index.recorded_pipeline_fingerprint,
            Some(produced.fingerprint().unwrap())
        );
        assert!(!index.add_with_pipeline("bad", vec![1.0, 0.0], &produced, false));
    }

    #[test]
    fn journal_replays_upserts_deletes_and_ignores_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("embeddings-v2.bin");
        let journal = dir.path().join("embeddings-v2.journal");
        let identity = test_identity();
        let pipeline = test_pipeline("model-a", 2);

        let mut original = EmbeddingIndex::new();
        assert!(original.add_with_pipeline("keep", vec![1.0, 0.0], &pipeline, false));
        assert!(original.add_with_pipeline("remove", vec![0.0, 1.0], &pipeline, false));
        original
            .save_binary_v2(&base, &identity, 1, &pipeline)
            .unwrap();
        original.mark_base_persisted();
        assert!(original.add_with_pipeline("added", vec![0.5, 0.5], &pipeline, false));
        let live = std::collections::HashSet::from(["keep".to_string(), "added".to_string()]);
        assert_eq!(original.retain_uids(&live), 1);
        assert_eq!(
            original
                .append_journal_v2(&journal, &identity, &pipeline)
                .unwrap(),
            2
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(&[99, 0, 0])
            .unwrap();

        let mut reopened = EmbeddingIndex::load_binary_v2(&base).unwrap();
        reopened
            .replay_journal_v2(&journal, &identity, &pipeline)
            .unwrap();
        assert_eq!(reopened.get("keep"), Some(vec![1.0, 0.0]));
        assert_eq!(reopened.get("added"), Some(vec![0.5, 0.5]));
        assert!(reopened.get("remove").is_none());

        assert!(reopened.add_with_pipeline("later", vec![0.25, 0.75], &pipeline, false));
        reopened
            .append_journal_v2(&journal, &identity, &pipeline)
            .unwrap();
        let mut final_index = EmbeddingIndex::load_binary_v2(&base).unwrap();
        final_index
            .replay_journal_v2(&journal, &identity, &pipeline)
            .unwrap();
        assert!(final_index.get("later").is_some());
        assert_eq!(final_index.len(), 3);
    }

    #[test]
    fn store_checkpoint_appends_journal_and_reopens_without_rewriting_base() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("journal-reopen.lbug");
        let pipeline = test_pipeline("model-a", 2);
        let base_before = dir.path().join("base-before.bin");
        let mut journal_name = db_path.as_os_str().to_owned();
        journal_name.push(".embeddings.journal");
        let journal_path = std::path::PathBuf::from(journal_name);

        {
            let store = GraphStore::create(&db_path).unwrap();
            store.set_embedding_pipeline(&pipeline).unwrap();
            assert!(store.add_embedding_with_pipeline("first", vec![1.0, 0.0], &pipeline, false));
            store.flush_embedding_index().unwrap();
            let base = store.embedding_sidecar_path().unwrap();
            std::fs::hard_link(&base, &base_before).unwrap();

            assert!(store.add_embedding_with_pipeline("second", vec![0.0, 1.0], &pipeline, false));
            store.flush_embedding_index().unwrap();
            assert!(
                journal_path.exists(),
                "routine checkpoint must use the journal"
            );
            assert_eq!(
                std::fs::read(&base).unwrap(),
                std::fs::read(&base_before).unwrap(),
                "routine checkpoint must not rewrite the corpus-sized base"
            );
        }

        let reopened = GraphStore::open(&db_path).unwrap();
        assert!(reopened.has_embedding("first"));
        assert!(reopened.has_embedding("second"));
        reopened.compact_embedding_index().unwrap();
        assert!(!journal_path.exists());
        drop(reopened);

        let compacted = GraphStore::open(&db_path).unwrap();
        assert!(compacted.has_embedding("first"));
        assert!(compacted.has_embedding("second"));
    }

    #[test]
    fn producer_patch_version_does_not_invalidate_a_compatible_base() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("producer-version.lbug");
        let pipeline = test_pipeline("model-a", 2);
        let base = {
            let store = GraphStore::create(&db_path).unwrap();
            store.set_embedding_pipeline(&pipeline).unwrap();
            assert!(store.add_embedding_with_pipeline(
                "persisted",
                vec![1.0, 0.0],
                &pipeline,
                false,
            ));
            store.flush_embedding_index().unwrap();
            store.embedding_sidecar_path().unwrap()
        };

        let mut bytes = std::fs::read(&base).unwrap();
        let current = env!("CARGO_PKG_VERSION").as_bytes();
        let replacement = vec![b'9'; current.len()];
        let offset = bytes
            .windows(current.len())
            .position(|window| window == current)
            .expect("producer version is encoded in the v2 envelope");
        bytes[offset..offset + current.len()].copy_from_slice(&replacement);
        std::fs::write(&base, bytes).unwrap();

        let reopened = GraphStore::open(&db_path).unwrap();
        assert!(
            reopened.has_embedding("persisted"),
            "producer package version is provenance, not compatibility"
        );
    }

    #[test]
    fn invalid_existing_base_is_replaced_on_the_next_successful_flush() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("invalid-base-recovery.lbug");
        let pipeline = test_pipeline("model-a", 2);
        let base = {
            let store = GraphStore::create(&db_path).unwrap();
            store.set_embedding_pipeline(&pipeline).unwrap();
            store.embedding_sidecar_path().unwrap()
        };
        std::fs::write(&base, b"legacy-or-corrupt-base").unwrap();

        for ordinal in 0..4 {
            let uid = format!("round-{ordinal}");
            let store = GraphStore::open(&db_path).unwrap();
            for previous in 0..ordinal {
                assert!(store.has_embedding(&format!("round-{previous}")));
            }
            assert!(store.add_embedding_with_pipeline(
                &uid,
                vec![1.0, ordinal as f32],
                &pipeline,
                false,
            ));
            store.flush_embedding_index().unwrap();
        }

        let reopened = GraphStore::open(&db_path).unwrap();
        for ordinal in 0..4 {
            assert!(reopened.has_embedding(&format!("round-{ordinal}")));
        }
        assert!(EmbeddingIndex::load_binary_v2(&base).is_ok());
    }

    #[test]
    fn bounded_vector_top_k_matches_full_sort_oracle_with_ties() {
        let mut index = EmbeddingIndex::new();
        for (uid, vector) in [
            ("z", vec![1.0, 0.0]),
            ("a", vec![1.0, 0.0]),
            ("b", vec![0.8, 0.2]),
            ("c", vec![0.5, 0.5]),
            ("d", vec![0.0, 1.0]),
        ] {
            assert!(index.add(uid, vector, false));
        }
        let query = [1.0, 0.0];
        let mut oracle: Vec<_> = index
            .embeddings
            .iter()
            .map(|(uid, vector)| (uid.clone(), cosine_similarity(&query, vector)))
            .collect();
        oracle.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        oracle.truncate(3);
        assert_eq!(index.vector_search(&query, 3), oracle);
        assert_eq!(index.vector_search(&query, 0), Vec::<(String, f64)>::new());
    }

    #[test]
    fn binary_save_replaces_the_sidecar_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.bin");
        let old_link = dir.path().join("old-embeddings.bin");

        let mut old = EmbeddingIndex::new();
        assert!(old.add("note:old", vec![1.0, 0.0], false));
        old.save_binary(&path).unwrap();
        std::fs::hard_link(&path, &old_link).unwrap();

        let mut new = EmbeddingIndex::new();
        assert!(new.add("head:new", vec![0.0, 1.0], false));
        new.save_binary(&path).unwrap();

        let current = EmbeddingIndex::load_binary(&path).unwrap();
        assert!(current.get("head:new").is_some());
        assert!(current.get("note:old").is_none());
        let old_snapshot = EmbeddingIndex::load_binary(&old_link).unwrap();
        assert!(old_snapshot.get("note:old").is_some());
        assert!(old_snapshot.get("head:new").is_none());
    }

    #[test]
    fn binary_atomic_replace_cleans_partial_temp_after_write_error() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.bin");
        std::fs::write(&path, b"previous-valid-sidecar").unwrap();

        let error = atomic_replace_file(&path, |file| {
            file.write_all(b"partial replacement")?;
            Err(std::io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected write failure"));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous-valid-sidecar");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn binary_save_reports_parent_sync_failure_and_reopens_complete_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.bin");
        let mut idx = EmbeddingIndex::new();
        assert!(idx.add("sym:durable", vec![1.0, 0.0], false));

        let error = crate::durable_sidecar::with_test_fault(
            crate::durable_sidecar::TestFault::ParentSync,
            || idx.save_binary(&path),
        )
        .unwrap_err();

        assert!(error.to_string().contains("sync parent after replacing"));
        let reopened = EmbeddingIndex::load_binary(&path).unwrap();
        assert_eq!(reopened.get("sym:durable"), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn binary_empty_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");

        let idx = EmbeddingIndex::new();
        idx.save_binary(&path).unwrap();

        let loaded = EmbeddingIndex::load_binary(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn binary_load_rejects_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.bin");
        std::fs::write(&path, b"BADMxxxxxxxxxxxx").unwrap();

        match EmbeddingIndex::load_binary(&path) {
            Ok(_) => panic!("should reject bad magic"),
            Err(e) => assert!(
                format!("{e}").contains("invalid embedding file magic"),
                "unexpected error: {e}",
            ),
        }
    }

    #[test]
    fn binary_load_rejects_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.bin");
        std::fs::write(&path, b"NWEM").unwrap(); // only 4 bytes, need 16

        assert!(EmbeddingIndex::load_binary(&path).is_err());
    }

    #[test]
    fn hybrid_search_with_vector_boosts_vector_hits() {
        let store = GraphStore::in_memory().unwrap();
        // Insert "greet" (matches text) and "farewell" (does not match text)
        store
            .insert_symbol(&make_symbol("sym:greet", "greet"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym:farewell", "farewell"))
            .unwrap();

        let mut idx = EmbeddingIndex::new();
        // farewell gets a perfect embedding match; greet gets a distant one
        assert!(idx.add("sym:farewell", vec![1.0, 0.0, 0.0], false));
        assert!(idx.add("sym:greet", vec![0.0, 1.0, 0.0], false));

        let query_vec = [1.0_f32, 0.0, 0.0];
        let results = store
            .hybrid_search(
                "greet",
                Some(&query_vec),
                Some(&idx),
                10,
                &empty_seed_resolution(),
            )
            .unwrap();

        // Both should appear (greet from text, farewell from vector)
        let uids: Vec<&str> = results.iter().map(|r| r.uid.as_str()).collect();
        assert!(uids.contains(&"sym:greet"), "greet should be in results");
        assert!(
            uids.contains(&"sym:farewell"),
            "farewell should be in results via vector"
        );
    }
}

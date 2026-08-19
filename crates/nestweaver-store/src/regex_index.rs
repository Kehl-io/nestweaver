//! Disposable, per-scope Tantivy acceleration for conservative regex search.
//!
//! One Tantivy document represents one graph candidate. The graph remains the
//! source of truth: callers validate the committed metadata against the graph
//! snapshot and run Rust `regex` over hydrated text before returning a match.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, Value};
use tantivy::{Index, ReloadPolicy, TantivyDocument, Term, doc};

use crate::error::StoreError;

pub const REGEX_INDEX_SCHEMA_VERSION: u32 = 3;
pub const REGEX_TOKENIZER_FINGERPRINT: &str =
    "nestweaver-unicode-scalar-lowercase-distinct-trigram-v1";
const METADATA_KIND: &str = "__nestweaver_regex_metadata__";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegexShardMetadata {
    pub schema_version: u32,
    pub tokenizer_fingerprint: String,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub source_graph_generation: u64,
    pub scope_uid: String,
    pub scope_epoch: u64,
    pub candidate_count: usize,
    pub candidate_digest: String,
}

impl RegexShardMetadata {
    pub fn algorithm_fingerprint(&self) -> String {
        format!(
            "regex-v{}:{}",
            self.schema_version, self.tokenizer_fingerprint
        )
    }
}

#[derive(Debug, Clone)]
pub struct RegexShardDocument<'a> {
    pub uid: &'a str,
    pub kind: &'a str,
    pub text_hash: &'a str,
    pub trigrams: &'a HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CurrentPointer {
    version: u32,
    generation: String,
    metadata: RegexShardMetadata,
    checksum: String,
}

impl CurrentPointer {
    fn new(generation: String, metadata: RegexShardMetadata) -> Result<Self, StoreError> {
        let mut pointer = Self {
            version: 1,
            generation,
            metadata,
            checksum: String::new(),
        };
        pointer.checksum = pointer.expected_checksum()?;
        Ok(pointer)
    }

    fn expected_checksum(&self) -> Result<String, StoreError> {
        let bytes =
            serde_json::to_vec(&(self.version, self.generation.as_str(), &self.metadata))
                .map_err(|error| StoreError::Query(format!("serialize regex pointer: {error}")))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.version != 1 {
            return Err(StoreError::Query(format!(
                "unsupported regex shard pointer version {}",
                self.version
            )));
        }
        if self.generation.is_empty()
            || self.generation.contains('/')
            || self.generation.contains('\\')
            || self.generation == "."
            || self.generation == ".."
        {
            return Err(StoreError::Query(
                "invalid regex shard generation name".to_string(),
            ));
        }
        if self.checksum != self.expected_checksum()? {
            return Err(StoreError::Query(
                "regex shard CURRENT checksum mismatch".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Fields {
    uid: Field,
    kind: Field,
    trigram: Field,
    text_hash: Field,
    metadata: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let uid = builder.add_text_field("uid", STRING | STORED);
    let kind = builder.add_text_field("kind", STRING | STORED);
    let trigram = builder.add_text_field("trigram", STRING);
    let text_hash = builder.add_text_field("text_hash", STORED);
    let metadata = builder.add_text_field("metadata", STORED);
    let schema = builder.build();
    (
        schema,
        Fields {
            uid,
            kind,
            trigram,
            text_hash,
            metadata,
        },
    )
}

fn inspect_fields(schema: &Schema) -> Result<Fields, StoreError> {
    let field = |name: &str| {
        schema
            .get_field(name)
            .map_err(|error| StoreError::Query(format!("regex shard missing {name}: {error}")))
    };
    Ok(Fields {
        uid: field("uid")?,
        kind: field("kind")?,
        trigram: field("trigram")?,
        text_hash: field("text_hash")?,
        metadata: field("metadata")?,
    })
}

fn scope_hash(scope_uid: &str) -> String {
    blake3::hash(scope_uid.as_bytes()).to_hex().to_string()
}

fn extract_text(document: &TantivyDocument, field: Field) -> String {
    document
        .get_first(field)
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn read_metadata(index: &Index, fields: Fields) -> Result<(RegexShardMetadata, usize), StoreError> {
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|error| StoreError::Query(format!("open regex shard reader: {error}")))?;
    let searcher = reader.searcher();
    let metadata_query = TermQuery::new(
        Term::from_field_text(fields.kind, METADATA_KIND),
        IndexRecordOption::Basic,
    );
    let hits = searcher
        .search(&metadata_query, &TopDocs::with_limit(2).order_by_score())
        .map_err(|error| StoreError::Query(format!("read regex shard metadata: {error}")))?;
    let mut metadata = None;
    for (_, address) in hits {
        let document: TantivyDocument = searcher
            .doc(address)
            .map_err(|error| StoreError::Query(format!("decode regex shard metadata: {error}")))?;
        if extract_text(&document, fields.kind) == METADATA_KIND {
            let encoded = extract_text(&document, fields.metadata);
            metadata = Some(serde_json::from_str(&encoded).map_err(|error| {
                StoreError::Query(format!("parse regex shard metadata: {error}"))
            })?);
            break;
        }
    }
    let metadata = metadata
        .ok_or_else(|| StoreError::Query("regex shard metadata document is missing".to_string()))?;
    let candidate_count = searcher.num_docs().saturating_sub(1) as usize;
    Ok((metadata, candidate_count))
}

/// Filesystem manager for immutable per-scope regex shard generations.
#[derive(Debug, Clone)]
pub struct RegexIndex {
    root: PathBuf,
}

impl RegexIndex {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn scope_root(&self, scope_uid: &str) -> PathBuf {
        self.root.join("scopes").join(scope_hash(scope_uid))
    }

    fn current_pointer(&self, scope_uid: &str) -> Result<Option<CurrentPointer>, StoreError> {
        let path = self.scope_root(scope_uid).join("CURRENT");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StoreError::Query(format!(
                    "read regex shard pointer {}: {error}",
                    path.display()
                )));
            }
        };
        let pointer: CurrentPointer = serde_json::from_slice(&bytes).map_err(|error| {
            StoreError::Query(format!(
                "parse regex shard pointer {}: {error}",
                path.display()
            ))
        })?;
        pointer.validate()?;
        if pointer.metadata.scope_uid != scope_uid {
            return Err(StoreError::Query(format!(
                "regex shard hash collision: expected scope {scope_uid}, found {}",
                pointer.metadata.scope_uid
            )));
        }
        Ok(Some(pointer))
    }

    fn open_current(
        &self,
        scope_uid: &str,
    ) -> Result<Option<(Index, Fields, RegexShardMetadata)>, StoreError> {
        let Some(pointer) = self.current_pointer(scope_uid)? else {
            return Ok(None);
        };
        let path = self
            .scope_root(scope_uid)
            .join("generations")
            .join(&pointer.generation);
        let index = Index::open_in_dir(&path).map_err(|error| {
            StoreError::Query(format!("open regex shard {}: {error}", path.display()))
        })?;
        let fields = inspect_fields(&index.schema())?;
        let (observed, count) = read_metadata(&index, fields)?;
        if observed != pointer.metadata || count != observed.candidate_count {
            return Err(StoreError::Query(format!(
                "regex shard metadata/content mismatch for scope {scope_uid}"
            )));
        }
        Ok(Some((index, fields, observed)))
    }

    /// Build and validate a complete sibling generation, then durably select it.
    pub fn replace_scope(
        &self,
        metadata: RegexShardMetadata,
        documents: &[RegexShardDocument<'_>],
    ) -> Result<(), StoreError> {
        if metadata.schema_version != REGEX_INDEX_SCHEMA_VERSION
            || metadata.tokenizer_fingerprint != REGEX_TOKENIZER_FINGERPRINT
            || metadata.candidate_count != documents.len()
        {
            return Err(StoreError::Query(
                "invalid regex shard build metadata".to_string(),
            ));
        }

        let scope_root = self.scope_root(&metadata.scope_uid);
        let generations = scope_root.join("generations");
        std::fs::create_dir_all(&generations).map_err(|error| {
            StoreError::Query(format!(
                "create regex shard directory {}: {error}",
                generations.display()
            ))
        })?;
        let staging = tempfile::Builder::new()
            .prefix(".building-")
            .tempdir_in(&generations)
            .map_err(|error| StoreError::Query(format!("create regex shard staging: {error}")))?;
        let (schema, fields) = build_schema();
        let index = Index::create_in_dir(staging.path(), schema)
            .map_err(|error| StoreError::Query(format!("create regex shard: {error}")))?;
        let mut writer = index
            .writer(50_000_000)
            .map_err(|error| StoreError::Query(format!("open regex shard writer: {error}")))?;

        let encoded_metadata = serde_json::to_string(&metadata)
            .map_err(|error| StoreError::Query(format!("serialize regex metadata: {error}")))?;
        writer
            .add_document(doc!(
                fields.uid => format!("meta:{}", metadata.scope_uid),
                fields.kind => METADATA_KIND.to_string(),
                fields.metadata => encoded_metadata,
            ))
            .map_err(|error| StoreError::Query(format!("write regex metadata: {error}")))?;
        for document in documents {
            let mut tantivy_document = TantivyDocument::default();
            tantivy_document.add_text(fields.uid, document.uid);
            tantivy_document.add_text(fields.kind, document.kind);
            tantivy_document.add_text(fields.text_hash, document.text_hash);
            for trigram in document.trigrams {
                tantivy_document.add_text(fields.trigram, trigram);
            }
            writer.add_document(tantivy_document).map_err(|error| {
                StoreError::Query(format!("write regex candidate {}: {error}", document.uid))
            })?;
        }
        writer
            .commit()
            .map_err(|error| StoreError::Query(format!("commit regex shard: {error}")))?;
        writer
            .wait_merging_threads()
            .map_err(|error| StoreError::Query(format!("finish regex shard merge: {error}")))?;
        drop(index);

        let verification = Index::open_in_dir(staging.path())
            .map_err(|error| StoreError::Query(format!("reopen staged regex shard: {error}")))?;
        let verified_fields = inspect_fields(&verification.schema())?;
        let (verified_metadata, verified_count) = read_metadata(&verification, verified_fields)?;
        if verified_metadata != metadata || verified_count != documents.len() {
            return Err(StoreError::Query(
                "staged regex shard failed metadata/count validation".to_string(),
            ));
        }
        drop(verification);

        let generation = format!(
            "{}-{}-{}",
            metadata.scope_epoch,
            &metadata.candidate_digest[..metadata.candidate_digest.len().min(16)],
            uuid::Uuid::new_v4()
        );
        let generation_path = generations.join(&generation);
        if generation_path.exists() {
            std::fs::remove_dir_all(&generation_path).map_err(|error| {
                StoreError::Query(format!("retire incomplete regex generation: {error}"))
            })?;
        }
        let staging_path = staging.keep();
        std::fs::rename(&staging_path, &generation_path).map_err(|error| {
            StoreError::Query(format!("publish regex shard generation: {error}"))
        })?;
        crate::durable_sidecar::sync_parent_directory_durable(&generation_path)
            .map_err(|error| StoreError::Query(format!("sync regex shard generation: {error}")))?;

        let pointer = CurrentPointer::new(generation, metadata)?;
        let pointer_path = scope_root.join("CURRENT");
        let bytes = serde_json::to_vec_pretty(&pointer)
            .map_err(|error| StoreError::Query(format!("serialize regex pointer: {error}")))?;
        crate::durable_sidecar::atomic_replace_file(&pointer_path, |file| file.write_all(&bytes))
            .map_err(|error| StoreError::Query(format!("publish regex pointer: {error}")))?;
        Ok(())
    }

    /// Return exact candidate UIDs from a trusted shard. Metadata mismatch is
    /// explicit so callers can widen to a graph scan for this scope.
    pub fn candidate_uids(
        &self,
        expected: &RegexShardMetadata,
        clauses: &[HashSet<String>],
        cap: usize,
    ) -> Result<Option<HashSet<String>>, StoreError> {
        let Some((index, fields, observed)) = self.open_current(&expected.scope_uid)? else {
            return Ok(None);
        };
        if &observed != expected {
            return Ok(None);
        }
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|error| StoreError::Query(format!("open regex shard reader: {error}")))?;
        let searcher = reader.searcher();

        let mut required: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(clauses.len());
        for clause in clauses {
            let alternatives: Vec<(Occur, Box<dyn Query>)> = clause
                .iter()
                .map(|trigram| {
                    let term = Term::from_field_text(fields.trigram, trigram);
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
                    )
                })
                .collect();
            required.push((Occur::Must, Box::new(BooleanQuery::new(alternatives))));
        }
        let query = BooleanQuery::new(required);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(cap).order_by_score())
            .map_err(|error| StoreError::Query(format!("query regex shard: {error}")))?;
        let mut uids = HashSet::with_capacity(hits.len());
        for (_, address) in hits {
            let document: TantivyDocument = searcher.doc(address).map_err(|error| {
                StoreError::Query(format!("decode regex shard candidate: {error}"))
            })?;
            let uid = extract_text(&document, fields.uid);
            if !uid.is_empty() {
                uids.insert(uid);
            }
        }
        Ok(Some(uids))
    }

    pub fn metadata(&self, scope_uid: &str) -> Result<Option<RegexShardMetadata>, StoreError> {
        self.open_current(scope_uid)
            .map(|opened| opened.map(|(_, _, metadata)| metadata))
    }

    /// Read the committed UID/content-hash manifest from one shard. This is
    /// bounded by the metadata count and avoids any graph posting structure.
    pub fn document_hashes(
        &self,
        scope_uid: &str,
    ) -> Result<Option<std::collections::HashMap<String, String>>, StoreError> {
        let Some((index, fields, metadata)) = self.open_current(scope_uid)? else {
            return Ok(None);
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|error| StoreError::Query(format!("open regex shard reader: {error}")))?;
        let searcher = reader.searcher();
        let hits = searcher
            .search(
                &AllQuery,
                &TopDocs::with_limit(metadata.candidate_count.saturating_add(1)).order_by_score(),
            )
            .map_err(|error| StoreError::Query(format!("read regex shard documents: {error}")))?;
        let mut hashes = std::collections::HashMap::with_capacity(metadata.candidate_count);
        for (_, address) in hits {
            let document: TantivyDocument = searcher.doc(address).map_err(|error| {
                StoreError::Query(format!("decode regex shard document: {error}"))
            })?;
            if extract_text(&document, fields.kind) == METADATA_KIND {
                continue;
            }
            let uid = extract_text(&document, fields.uid);
            let text_hash = extract_text(&document, fields.text_hash);
            if uid.is_empty() || text_hash.is_empty() || hashes.insert(uid, text_hash).is_some() {
                return Err(StoreError::Query(format!(
                    "regex shard {scope_uid} has malformed or duplicate document identity"
                )));
            }
        }
        if hashes.len() != metadata.candidate_count {
            return Err(StoreError::Query(format!(
                "regex shard {scope_uid} document manifest count mismatch"
            )));
        }
        Ok(Some(hashes))
    }

    /// Retire a removed scope by durably unlinking only its selector. Existing
    /// readers may finish against immutable generation files; cleanup is a
    /// separate retention operation.
    pub fn retire_scope(&self, scope_uid: &str) -> Result<bool, StoreError> {
        let current = self.scope_root(scope_uid).join("CURRENT");
        crate::durable_sidecar::remove_file_durable_if_exists(&current)
            .map_err(|error| StoreError::Query(format!("retire regex scope {scope_uid}: {error}")))
    }

    /// Inspect every currently selected shard without trusting directory names
    /// as scope identity. Corrupt entries are skipped by callers only when they
    /// can widen the corresponding graph scope to a scan.
    pub fn list_metadata(&self) -> Result<Vec<RegexShardMetadata>, StoreError> {
        let scopes = self.root.join("scopes");
        let entries = match std::fs::read_dir(&scopes) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(StoreError::Query(format!(
                    "list regex shard root {}: {error}",
                    scopes.display()
                )));
            }
        };
        let mut metadata = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                StoreError::Query(format!("read regex shard directory entry: {error}"))
            })?;
            if !entry
                .file_type()
                .map_err(|error| StoreError::Query(format!("inspect regex shard: {error}")))?
                .is_dir()
            {
                continue;
            }
            let bytes = match std::fs::read(entry.path().join("CURRENT")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(StoreError::Query(format!(
                        "read regex shard CURRENT: {error}"
                    )));
                }
            };
            let pointer: CurrentPointer = serde_json::from_slice(&bytes).map_err(|error| {
                StoreError::Query(format!("parse regex shard CURRENT: {error}"))
            })?;
            pointer.validate()?;
            if scope_hash(&pointer.metadata.scope_uid) != entry.file_name().to_string_lossy() {
                return Err(StoreError::Query(format!(
                    "regex shard directory does not match scope {}",
                    pointer.metadata.scope_uid
                )));
            }
            metadata.push(pointer.metadata);
        }
        metadata.sort_by(|left, right| left.scope_uid.cmp(&right.scope_uid));
        Ok(metadata)
    }
}

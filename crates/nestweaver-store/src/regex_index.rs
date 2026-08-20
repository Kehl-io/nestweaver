//! Disposable, per-scope Tantivy acceleration for conservative regex search.
//!
//! One Tantivy document represents one graph candidate. The graph remains the
//! source of truth: callers validate the committed metadata against the graph
//! snapshot and run Rust `regex` over hydrated text before returning a match.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexScopeIssue {
    pub scope_hash: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegexGarbageCollectionReport {
    pub removed: usize,
    pub failures: Vec<RegexScopeIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegexMetadataReport {
    pub metadata: Vec<RegexShardMetadata>,
    pub failures: Vec<RegexScopeIssue>,
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

#[derive(Clone)]
struct CachedShard {
    generation: String,
    index: Index,
    fields: Fields,
    metadata: RegexShardMetadata,
}

/// Process-local, descriptor-bounded cache of opened scope indexes.
/// Searchers remain snapshot-isolated; the cache only avoids reopening the
/// same directory and is cleared before generation garbage collection.
pub(crate) struct RegexReaderPool {
    shards: Mutex<lru::LruCache<String, CachedShard>>,
}

impl Default for RegexReaderPool {
    fn default() -> Self {
        Self {
            shards: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(32).expect("non-zero regex reader pool capacity"),
            )),
        }
    }
}

impl RegexReaderPool {
    fn clear(&self) {
        self.shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
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

pub(crate) fn scope_hash(scope_uid: &str) -> String {
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
#[derive(Clone)]
pub struct RegexIndex {
    root: PathBuf,
    readers: Arc<RegexReaderPool>,
}

impl RegexIndex {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            readers: Arc::new(RegexReaderPool::default()),
        }
    }

    pub(crate) fn with_reader_pool(
        root: impl Into<PathBuf>,
        readers: Arc<RegexReaderPool>,
    ) -> Self {
        Self {
            root: root.into(),
            readers,
        }
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
        if let Some(cached) = self
            .readers
            .shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(scope_uid)
            .filter(|cached| {
                cached.generation == pointer.generation && cached.metadata == pointer.metadata
            })
            .cloned()
        {
            return Ok(Some((cached.index, cached.fields, cached.metadata)));
        }
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
        self.readers
            .shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .put(
                scope_uid.to_string(),
                CachedShard {
                    generation: pointer.generation,
                    index: index.clone(),
                    fields,
                    metadata: observed.clone(),
                },
            );
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

    /// Apply an exact document delta to the currently selected scope.
    ///
    /// Tantivy commits are atomic for readers. `CURRENT` remains bound to the
    /// previous metadata until the commit has been reopened and validated; in
    /// that short interval the graph/sidecar metadata mismatch makes callers
    /// scan this scope. A crash can therefore make the disposable shard stale,
    /// but can never make search trust a partial update.
    pub fn update_scope(
        &self,
        previous: &RegexShardMetadata,
        metadata: RegexShardMetadata,
        upserts: &[RegexShardDocument<'_>],
        deletes: &[String],
    ) -> Result<(), StoreError> {
        if metadata.schema_version != REGEX_INDEX_SCHEMA_VERSION
            || metadata.tokenizer_fingerprint != REGEX_TOKENIZER_FINGERPRINT
            || metadata.scope_uid != previous.scope_uid
        {
            return Err(StoreError::Query(
                "invalid incremental regex shard metadata".to_string(),
            ));
        }
        let pointer = self
            .current_pointer(&metadata.scope_uid)?
            .ok_or_else(|| StoreError::Query("regex shard CURRENT is missing".to_string()))?;
        if pointer.metadata != *previous {
            return Err(StoreError::Query(format!(
                "regex scope {} advanced before incremental publication",
                metadata.scope_uid
            )));
        }
        let path = self
            .scope_root(&metadata.scope_uid)
            .join("generations")
            .join(&pointer.generation);
        let index = Index::open_in_dir(&path).map_err(|error| {
            StoreError::Query(format!("open regex shard {}: {error}", path.display()))
        })?;
        let fields = inspect_fields(&index.schema())?;
        let (observed, _) = read_metadata(&index, fields)?;
        if observed != *previous {
            return Err(StoreError::Query(format!(
                "regex scope {} metadata changed before incremental publication",
                metadata.scope_uid
            )));
        }

        let mut writer = index
            .writer(50_000_000)
            .map_err(|error| StoreError::Query(format!("open regex shard writer: {error}")))?;
        for uid in deletes
            .iter()
            .map(String::as_str)
            .chain(upserts.iter().map(|document| document.uid))
        {
            writer.delete_term(Term::from_field_text(fields.uid, uid));
        }
        writer.delete_term(Term::from_field_text(
            fields.uid,
            &format!("meta:{}", metadata.scope_uid),
        ));
        let encoded_metadata = serde_json::to_string(&metadata)
            .map_err(|error| StoreError::Query(format!("serialize regex metadata: {error}")))?;
        writer
            .add_document(doc!(
                fields.uid => format!("meta:{}", metadata.scope_uid),
                fields.kind => METADATA_KIND.to_string(),
                fields.metadata => encoded_metadata,
            ))
            .map_err(|error| StoreError::Query(format!("write regex metadata: {error}")))?;
        for document in upserts {
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
            .map_err(|error| StoreError::Query(format!("commit regex shard delta: {error}")))?;
        writer
            .wait_merging_threads()
            .map_err(|error| StoreError::Query(format!("finish regex shard merge: {error}")))?;
        drop(index);

        let verification = Index::open_in_dir(&path)
            .map_err(|error| StoreError::Query(format!("reopen regex shard: {error}")))?;
        let verified_fields = inspect_fields(&verification.schema())?;
        let (verified_metadata, verified_count) = read_metadata(&verification, verified_fields)?;
        if verified_metadata != metadata || verified_count != metadata.candidate_count {
            return Err(StoreError::Query(
                "incremental regex shard failed metadata/count validation".to_string(),
            ));
        }
        drop(verification);

        let pointer = CurrentPointer::new(pointer.generation, metadata)?;
        let pointer_path = self.scope_root(&pointer.metadata.scope_uid).join("CURRENT");
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

        // OR-of-ANDs (nw-142). `clauses` is a DNF: each entry is one alternation
        // branch whose trigrams are CONJUNCTS, and the branches are alternatives.
        // Building the inner clause with Should (the previous shape) meant a
        // document matched on any ONE shared trigram, which selected ~40% of the
        // corpus for a 20-character identifier.
        //
        // Trigrams are sorted so the emitted query is deterministic for a given
        // input; HashSet iteration order is not stable across runs.
        let mut branches: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(clauses.len());
        for clause in clauses {
            let mut trigrams: Vec<&String> = clause.iter().collect();
            trigrams.sort();
            let conjuncts: Vec<(Occur, Box<dyn Query>)> = trigrams
                .into_iter()
                .map(|trigram| {
                    let term = Term::from_field_text(fields.trigram, trigram);
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
                    )
                })
                .collect();
            branches.push((Occur::Should, Box::new(BooleanQuery::new(conjuncts))));
        }
        // A single branch needs no disjunction wrapper: emit the conjunction
        // directly so the common case (a plain literal) is one flat AND query.
        let query: Box<dyn Query> = if branches.len() == 1 {
            branches.pop().expect("length checked").1
        } else {
            Box::new(BooleanQuery::new(branches))
        };
        // Tantivy's collector reports only the retained hits, not whether more
        // matches existed. Probe one past the caller's budget so saturation is
        // explicit and the caller can conservatively widen this scope to the
        // graph instead of silently dropping regex matches.
        let hits = searcher
            .search(
                &*query,
                &TopDocs::with_limit(cap.saturating_add(1)).order_by_score(),
            )
            .map_err(|error| StoreError::Query(format!("query regex shard: {error}")))?;
        if hits.len() > cap {
            return Ok(None);
        }
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
        self.readers.clear();
        let scope_root = self.scope_root(scope_uid);
        let current = scope_root.join("CURRENT");
        let selector_removed = crate::durable_sidecar::remove_file_durable_if_exists(&current)
            .map_err(|error| {
                StoreError::Query(format!("retire regex scope {scope_uid}: {error}"))
            })?;
        Ok(selector_removed)
    }

    /// Remove unselected generations and selector-less retired scopes.
    ///
    /// This is deliberately separate from publication: a reader that still
    /// holds an old generation may delay deletion on Windows. The selected
    /// generation is never removed, and a corrupt selector fails closed
    /// rather than guessing which generation is live. A later refresh retries
    /// cleanup, so transient descriptor pressure cannot lose the shard.
    pub fn garbage_collect(&self) -> Result<RegexGarbageCollectionReport, StoreError> {
        self.readers.clear();
        let scopes = self.root.join("scopes");
        let entries = match std::fs::read_dir(&scopes) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegexGarbageCollectionReport::default());
            }
            Err(error) => {
                return Err(StoreError::Query(format!(
                    "list regex shards for garbage collection: {error}"
                )));
            }
        };
        let mut report = RegexGarbageCollectionReport::default();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: "<unknown>".to_string(),
                        error: format!("read regex shard for garbage collection: {error}"),
                    });
                    continue;
                }
            };
            let entry_hash = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!("inspect regex shard: {error}"),
                    });
                    continue;
                }
            }
            let scope_root = entry.path();
            let current_path = scope_root.join("CURRENT");
            let bytes = match std::fs::read(&current_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::remove_dir_all(&scope_root) {
                        Ok(()) => report.removed += 1,
                        Err(error) => report.failures.push(RegexScopeIssue {
                            scope_hash: entry_hash,
                            error: format!(
                                "remove retired regex shard {}: {error}",
                                scope_root.display()
                            ),
                        }),
                    }
                    continue;
                }
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!(
                            "read regex shard selector {}: {error}",
                            current_path.display()
                        ),
                    });
                    continue;
                }
            };
            let pointer: CurrentPointer = match serde_json::from_slice(&bytes) {
                Ok(pointer) => pointer,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!(
                            "refusing regex garbage collection with corrupt selector {}: {error}",
                            current_path.display()
                        ),
                    });
                    continue;
                }
            };
            if let Err(error) = pointer.validate() {
                report.failures.push(RegexScopeIssue {
                    scope_hash: entry_hash,
                    error: error.to_string(),
                });
                continue;
            }
            if scope_hash(&pointer.metadata.scope_uid) != entry_hash {
                report.failures.push(RegexScopeIssue {
                    scope_hash: entry_hash,
                    error: format!(
                        "refusing regex garbage collection after scope-hash collision for {}",
                        pointer.metadata.scope_uid
                    ),
                });
                continue;
            }
            let generations = scope_root.join("generations");
            let generation_entries = match std::fs::read_dir(&generations) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!("list regex generations {}: {error}", generations.display()),
                    });
                    continue;
                }
            };
            for generation in generation_entries {
                let generation = match generation {
                    Ok(generation) => generation,
                    Err(error) => {
                        report.failures.push(RegexScopeIssue {
                            scope_hash: entry_hash.clone(),
                            error: format!("read regex generation: {error}"),
                        });
                        continue;
                    }
                };
                if generation.file_name() == std::ffi::OsStr::new(&pointer.generation) {
                    continue;
                }
                let path = generation.path();
                match generation.file_type() {
                    Ok(kind) if !kind.is_dir() => continue,
                    Err(error) => {
                        report.failures.push(RegexScopeIssue {
                            scope_hash: entry_hash.clone(),
                            error: format!("inspect regex generation: {error}"),
                        });
                        continue;
                    }
                    Ok(_) => {}
                }
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => report.removed += 1,
                    Err(error) => report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash.clone(),
                        error: format!(
                            "remove unselected regex generation {}: {error}",
                            path.display()
                        ),
                    }),
                }
            }
        }
        crate::durable_sidecar::sync_parent_directory_durable(&scopes).map_err(|error| {
            StoreError::Query(format!("sync regex garbage collection: {error}"))
        })?;
        Ok(report)
    }

    /// Inspect every currently selected shard without trusting directory names
    /// as scope identity. Corrupt entries are skipped by callers only when they
    /// can widen the corresponding graph scope to a scan.
    pub fn list_metadata(&self) -> Result<RegexMetadataReport, StoreError> {
        let scopes = self.root.join("scopes");
        let entries = match std::fs::read_dir(&scopes) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegexMetadataReport::default());
            }
            Err(error) => {
                return Err(StoreError::Query(format!(
                    "list regex shard root {}: {error}",
                    scopes.display()
                )));
            }
        };
        let mut report = RegexMetadataReport::default();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: "<unknown>".to_string(),
                        error: format!("read regex shard directory entry: {error}"),
                    });
                    continue;
                }
            };
            let entry_hash = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!("inspect regex shard: {error}"),
                    });
                    continue;
                }
            }
            let bytes = match std::fs::read(entry.path().join("CURRENT")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!("read regex shard CURRENT: {error}"),
                    });
                    continue;
                }
            };
            let pointer: CurrentPointer = match serde_json::from_slice(&bytes) {
                Ok(pointer) => pointer,
                Err(error) => {
                    report.failures.push(RegexScopeIssue {
                        scope_hash: entry_hash,
                        error: format!("parse regex shard CURRENT: {error}"),
                    });
                    continue;
                }
            };
            if let Err(error) = pointer.validate() {
                report.failures.push(RegexScopeIssue {
                    scope_hash: entry_hash,
                    error: error.to_string(),
                });
                continue;
            }
            if scope_hash(&pointer.metadata.scope_uid) != entry_hash {
                report.failures.push(RegexScopeIssue {
                    scope_hash: entry_hash,
                    error: format!(
                        "regex shard directory does not match scope {}",
                        pointer.metadata.scope_uid
                    ),
                });
                continue;
            }
            report.metadata.push(pointer.metadata);
        }
        report
            .metadata
            .sort_by(|left, right| left.scope_uid.cmp(&right.scope_uid));
        report
            .failures
            .sort_by(|left, right| left.scope_hash.cmp(&right.scope_hash));
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(scope_epoch: u64, count: usize, digest: &str) -> RegexShardMetadata {
        RegexShardMetadata {
            schema_version: REGEX_INDEX_SCHEMA_VERSION,
            tokenizer_fingerprint: REGEX_TOKENIZER_FINGERPRINT.to_string(),
            brain_uuid: "brain".to_string(),
            publication_uuid: "publication".to_string(),
            source_graph_generation: scope_epoch,
            scope_uid: "repo:test".to_string(),
            scope_epoch,
            candidate_count: count,
            candidate_digest: digest.to_string(),
        }
    }

    #[test]
    fn exact_delta_updates_current_generation_without_rebuilding_the_scope() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let first_trigrams = HashSet::from(["alp".to_string(), "lph".to_string()]);
        let first = RegexShardDocument {
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &first_trigrams,
        };
        let initial = metadata(1, 1, "digest-one");
        index.replace_scope(initial.clone(), &[first]).unwrap();
        let generation = index
            .current_pointer("repo:test")
            .unwrap()
            .unwrap()
            .generation;

        let second_trigrams = HashSet::from(["bet".to_string(), "eta".to_string()]);
        let second = RegexShardDocument {
            uid: "sym:two",
            kind: "Symbol",
            text_hash: "hash-two",
            trigrams: &second_trigrams,
        };
        let updated = metadata(2, 1, "digest-two");
        index
            .update_scope(
                &initial,
                updated.clone(),
                &[second],
                &["sym:one".to_string()],
            )
            .unwrap();

        let pointer = index.current_pointer("repo:test").unwrap().unwrap();
        assert_eq!(pointer.generation, generation);
        assert_eq!(pointer.metadata, updated);
        assert_eq!(
            index.document_hashes("repo:test").unwrap().unwrap(),
            std::collections::HashMap::from([("sym:two".to_string(), "hash-two".to_string())])
        );
        assert_eq!(
            index
                .candidate_uids(&pointer.metadata, &[HashSet::from(["bet".to_string()])], 10,)
                .unwrap()
                .unwrap(),
            HashSet::from(["sym:two".to_string()])
        );
    }

    #[test]
    fn saturated_candidate_query_widens_instead_of_dropping_matches() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let uids = ["one", "two", "three", "four", "five"];
        let documents = uids
            .iter()
            .map(|uid| RegexShardDocument {
                uid,
                kind: "Symbol",
                text_hash: uid,
                trigrams: &trigrams,
            })
            .collect::<Vec<_>>();
        let published = metadata(1, documents.len(), "digest-five");
        index.replace_scope(published.clone(), &documents).unwrap();

        assert_eq!(
            index
                .candidate_uids(&published, &[HashSet::from(["alp".to_string()])], 2,)
                .unwrap(),
            None,
            "a saturated shard must widen to the graph scan oracle"
        );
        assert_eq!(
            index
                .candidate_uids(
                    &published,
                    &[HashSet::from(["alp".to_string()])],
                    documents.len(),
                )
                .unwrap()
                .unwrap()
                .len(),
            documents.len(),
            "an exact-cap result is complete and need not widen"
        );
    }

    /// nw-142: the trigrams of ONE literal are conjuncts. A document holding
    /// only some of them must NOT be selected. Before the fix the inner clause
    /// was built with Occur::Should, so any single shared trigram matched.
    #[test]
    fn a_branch_requires_all_of_its_trigrams() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        // Document contains "alp" but NOT "lph".
        let doc_trigrams = HashSet::from(["alp".to_string(), "zzz".to_string()]);
        let document = RegexShardDocument {
            uid: "sym:partial",
            kind: "Symbol",
            text_hash: "h",
            trigrams: &doc_trigrams,
        };
        let meta = metadata(1, 1, "digest");
        index.replace_scope(meta.clone(), &[document]).unwrap();

        // One branch requiring BOTH "alp" AND "lph": must not match.
        let branch = HashSet::from(["alp".to_string(), "lph".to_string()]);
        assert_eq!(
            index.candidate_uids(&meta, &[branch], 10).unwrap(),
            Some(HashSet::new()),
            "a branch is a conjunction; a partial trigram overlap must not select"
        );

        // A branch fully contained in the document must match.
        let satisfied = HashSet::from(["alp".to_string(), "zzz".to_string()]);
        assert_eq!(
            index.candidate_uids(&meta, &[satisfied], 10).unwrap(),
            Some(HashSet::from(["sym:partial".to_string()])),
        );
    }

    #[test]
    fn garbage_collection_keeps_only_selected_generations_and_retires_one_scope() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &trigrams,
        };
        index
            .replace_scope(
                metadata(1, 1, "digest-one"),
                std::slice::from_ref(&document),
            )
            .unwrap();
        index
            .replace_scope(metadata(2, 1, "digest-two"), &[document])
            .unwrap();
        let generations = index.scope_root("repo:test").join("generations");
        assert_eq!(std::fs::read_dir(&generations).unwrap().count(), 2);
        assert_eq!(index.garbage_collect().unwrap().removed, 1);
        assert_eq!(std::fs::read_dir(&generations).unwrap().count(), 1);

        assert!(index.retire_scope("repo:test").unwrap());
        assert!(index.scope_root("repo:test").exists());
        assert!(!index.scope_root("repo:test").join("CURRENT").exists());
        assert_eq!(index.garbage_collect().unwrap().removed, 1);
        assert!(!index.scope_root("repo:test").exists());
    }

    #[test]
    fn corrupt_selector_isolated_from_metadata_and_unrelated_garbage_collection() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &trigrams,
        };
        for scope_uid in ["repo:good", "repo:bad"] {
            let mut first = metadata(1, 1, "digest-one");
            first.scope_uid = scope_uid.to_string();
            index
                .replace_scope(first, std::slice::from_ref(&document))
                .unwrap();
            let mut second = metadata(2, 1, "digest-two");
            second.scope_uid = scope_uid.to_string();
            index
                .replace_scope(second, std::slice::from_ref(&document))
                .unwrap();
        }
        std::fs::write(index.scope_root("repo:bad").join("CURRENT"), b"{not-json").unwrap();

        let metadata = index.list_metadata().unwrap();
        assert_eq!(
            metadata
                .metadata
                .iter()
                .map(|entry| entry.scope_uid.as_str())
                .collect::<Vec<_>>(),
            vec!["repo:good"]
        );
        assert_eq!(metadata.failures.len(), 1);
        assert_eq!(metadata.failures[0].scope_hash, scope_hash("repo:bad"));

        let garbage_collection = index.garbage_collect().unwrap();
        assert_eq!(garbage_collection.removed, 1);
        assert_eq!(garbage_collection.failures.len(), 1);
        assert_eq!(
            garbage_collection.failures[0].scope_hash,
            scope_hash("repo:bad")
        );
        assert_eq!(
            std::fs::read_dir(index.scope_root("repo:good").join("generations"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_dir(index.scope_root("repo:bad").join("generations"))
                .unwrap()
                .count(),
            2,
            "a corrupt selector must preserve every generation in that scope"
        );
    }

    #[test]
    fn retiring_scope_leaves_generation_for_an_existing_reader() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &trigrams,
        };
        index
            .replace_scope(metadata(1, 1, "digest-one"), &[document])
            .unwrap();

        let (opened, _, _) = index.open_current("repo:test").unwrap().unwrap();
        let reader = opened
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        let searcher = reader.searcher();

        assert!(index.retire_scope("repo:test").unwrap());
        assert!(index.metadata("repo:test").unwrap().is_none());
        assert_eq!(
            searcher.num_docs(),
            2,
            "metadata plus one candidate remain readable"
        );
        assert!(index.scope_root("repo:test").exists());

        drop(searcher);
        drop(reader);
        drop(opened);
        assert_eq!(index.garbage_collect().unwrap().removed, 1);
        assert!(!index.scope_root("repo:test").exists());
    }

    #[test]
    fn reader_pool_evicts_under_many_scope_descriptor_pressure() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        for ordinal in 0..40 {
            let scope_uid = format!("repo:scope-{ordinal}");
            let trigrams = HashSet::from(["alp".to_string()]);
            let document = RegexShardDocument {
                uid: "sym:one",
                kind: "Symbol",
                text_hash: "hash-one",
                trigrams: &trigrams,
            };
            let mut scope_metadata = metadata(1, 1, &format!("digest-{ordinal}"));
            scope_metadata.scope_uid = scope_uid.clone();
            index.replace_scope(scope_metadata, &[document]).unwrap();
            assert!(index.metadata(&scope_uid).unwrap().is_some());
        }
        assert_eq!(index.readers.len(), 32);
        assert!(index.retire_scope("repo:scope-0").unwrap());
    }

    #[test]
    fn pointer_faults_preserve_an_old_or_complete_new_shard() {
        use crate::durable_sidecar::{TestFault, with_test_fault};

        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &trigrams,
        };
        let initial = metadata(1, 1, "digest-one");
        index
            .replace_scope(initial.clone(), std::slice::from_ref(&document))
            .unwrap();

        let staged = metadata(2, 1, "digest-two");
        let error = with_test_fault(TestFault::Persist, || {
            index.replace_scope(staged, std::slice::from_ref(&document))
        })
        .unwrap_err();
        assert!(error.to_string().contains("publish regex pointer"));
        assert_eq!(index.metadata("repo:test").unwrap(), Some(initial));
        assert_eq!(index.garbage_collect().unwrap().removed, 1);

        let published = metadata(3, 1, "digest-three");
        let error = with_test_fault(TestFault::ParentSync, || {
            index.replace_scope(published.clone(), &[document])
        })
        .unwrap_err();
        assert!(error.to_string().contains("publish regex pointer"));
        assert_eq!(index.metadata("repo:test").unwrap(), Some(published));

        let error =
            with_test_fault(TestFault::Remove, || index.retire_scope("repo:test")).unwrap_err();
        assert!(error.to_string().contains("retire regex scope"));
        assert!(index.metadata("repo:test").unwrap().is_some());
    }
}

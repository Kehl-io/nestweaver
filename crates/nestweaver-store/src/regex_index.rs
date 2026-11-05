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
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RegexQuery, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, Value};
use tantivy::{DocAddress, DocSet, Index, ReloadPolicy, TERMINATED, TantivyDocument, Term, doc};

use crate::error::StoreError;

/// Version 5 indexes the heading line with each section. Version 4 shards
/// stored only `Section.text_content`, which starts after the heading, so a
/// ready index missed `## Blockers` (nw-738). Search distrusts those shards
/// and scans them until `index --with-trigrams` rebuilds them. Version 6 also
/// stores distinct trigrams so incremental telemetry reads changed documents
/// rather than traversing the full inverted index.
pub const REGEX_INDEX_SCHEMA_VERSION: u32 = 6;
pub const REGEX_TOKENIZER_FINGERPRINT: &str =
    "nestweaver-unicode-scalar-lowercase-distinct-trigram-v1";
const METADATA_KIND: &str = "__nestweaver_regex_metadata__";
/// Durable tombstone written by [`RegexIndex::retire_scope`] before it removes
/// `CURRENT`. Its presence is the ONLY thing that licenses
/// [`RegexIndex::garbage_collect`] to delete a scope whose `CURRENT` is
/// absent; see `retire_scope` for why absence alone cannot mean that (nw-374).
const RETIRED_MARKER: &str = "RETIRED";

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
    /// Generations durably adopted as `CURRENT` because a crash left them
    /// complete but unselected (nw-374): see [`RegexIndex::garbage_collect`].
    pub adopted: usize,
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

/// Internal candidate identity. A note title and its frontmatter share a
/// public UID but are distinct documents in the regex shard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegexDocumentIdentity {
    pub kind: String,
    pub uid: String,
}

impl RegexDocumentIdentity {
    pub fn new(kind: &str, uid: &str) -> Self {
        Self {
            kind: kind.to_string(),
            uid: uid.to_string(),
        }
    }

    fn query(&self, fields: Fields) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.uid, &self.uid),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.kind, &self.kind),
                    IndexRecordOption::Basic,
                )),
            ),
        ]))
    }
}

#[derive(Debug, Clone)]
pub struct RegexShardDocument<'a> {
    pub uid: &'a str,
    pub kind: &'a str,
    pub path: String,
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

/// Recognize a complete quiet regex layout for retaining an incumbent restore
/// target. This is a PRESERVATION proof only: callers must never recursively
/// delete a tree on the strength of these descriptors. No graph file is opened
/// (closing one could release the restore caller's POSIX database locks).
pub fn is_preservable_live_layout(
    root: &Path,
    identity: &crate::PublicationIdentity,
    graph_generation: u64,
) -> anyhow::Result<bool> {
    const MAX_ENTRIES: usize = 100_000;
    const MAX_DESCRIPTOR: u64 = 8 * 1024 * 1024;
    fn inventory(path: &Path, depth: usize, total: &mut usize) -> anyhow::Result<bool> {
        *total += 1;
        if *total > MAX_ENTRIES || depth > 5 {
            return Ok(false);
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                if !inventory(&entry?.path(), depth + 1, total)? {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            if !metadata.is_file() {
                return Ok(false);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(metadata.nlink() == 1)
            }
            #[cfg(not(unix))]
            {
                Ok(false)
            }
        }
    }
    fn entries(path: &Path) -> anyhow::Result<Vec<std::fs::DirEntry>> {
        let values = std::fs::read_dir(path)?
            .take(MAX_ENTRIES + 1)
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            values.len() <= MAX_ENTRIES,
            "regex layout entry limit exceeded"
        );
        Ok(values)
    }
    fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
        use std::io::Read;
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() <= MAX_DESCRIPTOR,
            "unsupported regex descriptor"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_DESCRIPTOR + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_DESCRIPTOR,
            "oversized regex descriptor"
        );
        Ok(bytes)
    }
    fn metadata_supported(
        value: &RegexShardMetadata,
        identity: &crate::PublicationIdentity,
        generation: u64,
    ) -> bool {
        value.schema_version == REGEX_INDEX_SCHEMA_VERSION
            && value.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
            && value.brain_uuid == identity.brain_uuid
            && value.publication_uuid == identity.publication_uuid
            && value.source_graph_generation <= generation
            && !value.scope_uid.is_empty()
            && value.candidate_digest.len() == 64
            && value
                .candidate_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
    identity.validate()?;
    let mut entries_seen = 0;
    if !std::fs::symlink_metadata(root)?.is_dir() || !inventory(root, 0, &mut entries_seen)? {
        return Ok(false);
    }
    let top = entries(root)?;
    if top.len() != 1 || top[0].file_name() != "scopes" || !top[0].file_type()?.is_dir() {
        return Ok(false);
    }
    for scope in entries(&root.join("scopes"))? {
        if !scope.file_type()?.is_dir() {
            return Ok(false);
        }
        let contents = entries(&scope.path())?;
        if contents.len() != 2
            || !contents.iter().all(|entry| {
                let name = entry.file_name();
                name == "CURRENT" || name == "generations"
            })
        {
            return Ok(false);
        }
        if !std::fs::symlink_metadata(scope.path().join("generations"))?.is_dir() {
            return Ok(false);
        }
        let raw: serde_json::Value = serde_json::from_slice(&read(&scope.path().join("CURRENT"))?)?;
        if !raw.as_object().is_some_and(|object| {
            object.len() == 4
                && object.keys().all(|key| {
                    ["version", "generation", "metadata", "checksum"].contains(&key.as_str())
                })
        }) {
            return Ok(false);
        }
        if !raw
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|object| {
                object.len() == 9
                    && object.keys().all(|key| {
                        [
                            "schema_version",
                            "tokenizer_fingerprint",
                            "brain_uuid",
                            "publication_uuid",
                            "source_graph_generation",
                            "scope_uid",
                            "scope_epoch",
                            "candidate_count",
                            "candidate_digest",
                        ]
                        .contains(&key.as_str())
                    })
            })
        {
            return Ok(false);
        }
        let pointer: CurrentPointer = serde_json::from_value(raw)?;
        pointer.validate()?;
        if !metadata_supported(&pointer.metadata, identity, graph_generation)
            || scope.file_name() != scope_hash(&pointer.metadata.scope_uid).as_str()
        {
            return Ok(false);
        }
        let mut selected = false;
        for generation in entries(&scope.path().join("generations"))? {
            if !generation.file_type()?.is_dir() {
                return Ok(false);
            }
            let Some(name) = generation.file_name().to_str().map(str::to_owned) else {
                return Ok(false);
            };
            let mut parts = name.splitn(3, '-');
            let Some(epoch) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
                return Ok(false);
            };
            let Some(digest_prefix) = parts.next() else {
                return Ok(false);
            };
            if !parts
                .next()
                .is_some_and(|part| uuid::Uuid::parse_str(part).is_ok())
            {
                return Ok(false);
            }
            if !crate::tantivy_index::is_preservable_live_layout(&generation.path())? {
                return Ok(false);
            }
            // Open the already-inventoried Tantivy index read-only to bind its
            // metadata document to the selector. This never opens the graph.
            let index = Index::open_in_dir(generation.path())?;
            let fields = inspect_fields(&index.schema())?;
            let (metadata, count) = read_metadata(&index, fields)?;
            if !metadata_supported(&metadata, identity, graph_generation)
                || metadata.scope_uid != pointer.metadata.scope_uid
                || epoch != metadata.scope_epoch
                || epoch > pointer.metadata.scope_epoch
                || metadata
                    .candidate_digest
                    .get(..metadata.candidate_digest.len().min(16))
                    != Some(digest_prefix)
                || count != metadata.candidate_count
            {
                return Ok(false);
            }
            if name == pointer.generation {
                if metadata != pointer.metadata {
                    return Ok(false);
                }
                selected = true;
            }
        }
        if !selected {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Outcome of [`RegexIndex::recover_missing_current`].
enum MissingCurrentRecovery {
    /// The sole leftover generation was validated and durably selected.
    Adopted,
    /// No generation directory existed at all; nothing to lose by removing
    /// the (already-empty-of-data) scope directory.
    Empty,
}

#[derive(Clone, Copy)]
struct Fields {
    uid: Field,
    kind: Field,
    path: Field,
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
    maintenance: Mutex<()>,
}

impl Default for RegexReaderPool {
    fn default() -> Self {
        Self {
            maintenance: Mutex::new(()),
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
    let path = builder.add_text_field("path", STRING | STORED);
    let trigram = builder.add_text_field("trigram", STRING | STORED);
    let text_hash = builder.add_text_field("text_hash", STORED);
    let metadata = builder.add_text_field("metadata", STORED);
    let schema = builder.build();
    (
        schema,
        Fields {
            uid,
            kind,
            path,
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
        path: field("path")?,
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

/// Private generation outside scopes/: a concurrent retirement/GC cannot
/// reclaim it. Dropping a superseded preparation removes only this directory.
pub(crate) struct PreparedRegexScope {
    staging: tempfile::TempDir,
    pointer: CurrentPointer,
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
    #[cfg(test)]
    pub fn replace_scope(
        &self,
        metadata: RegexShardMetadata,
        documents: &[RegexShardDocument<'_>],
    ) -> Result<(), StoreError> {
        let prepared = self.prepare_replace_scope(metadata, documents)?;
        if self.publish_prepared(prepared)? {
            Ok(())
        } else {
            Err(StoreError::Query("regex maintenance is busy".into()))
        }
    }

    pub(crate) fn prepare_replace_scope(
        &self,
        metadata: RegexShardMetadata,
        documents: &[RegexShardDocument<'_>],
    ) -> Result<PreparedRegexScope, StoreError> {
        if metadata.schema_version != REGEX_INDEX_SCHEMA_VERSION
            || metadata.tokenizer_fingerprint != REGEX_TOKENIZER_FINGERPRINT
            || metadata.candidate_count != documents.len()
        {
            return Err(StoreError::Query(
                "invalid regex shard build metadata".to_string(),
            ));
        }

        std::fs::create_dir_all(&self.root)
            .map_err(|error| StoreError::Query(format!("create regex root: {error}")))?;
        let staging = tempfile::Builder::new()
            .prefix(".building-")
            .tempdir_in(&self.root)
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
            tantivy_document.add_text(fields.path, &document.path);
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
        // The code index has already committed. A first publication can still
        // see `meta.json` missing on this read (nw-744). That `NotFound` must
        // not fail the refresh; the open below is retried. A snapshot is
        // rewritten only after a read has succeeded and the path is still
        // absent. `meta.json` that names missing segments fails closed.
        let committed_meta = snapshot_staged_meta(&staging.path().join("meta.json"))?;
        drop(index);

        let verification = reopen_staged_regex_shard(staging.path(), committed_meta)?;
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
        Ok(PreparedRegexScope {
            staging,
            pointer: CurrentPointer::new(generation, metadata)?,
        })
    }

    /// Bounded selector publication. Never wait behind an unbounded sweep
    /// while holding the daemon gate; the unacknowledged scope stays queued.
    pub(crate) fn publish_prepared(
        &self,
        prepared: PreparedRegexScope,
    ) -> Result<bool, StoreError> {
        let _maintenance = match self.readers.maintenance.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
        };
        let scope_root = self.scope_root(&prepared.pointer.metadata.scope_uid);
        let generations = scope_root.join("generations");
        std::fs::create_dir_all(&generations).map_err(|error| {
            StoreError::Query(format!("create regex generation parent: {error}"))
        })?;
        let path = generations.join(&prepared.pointer.generation);
        std::fs::rename(prepared.staging.path(), &path)
            .map_err(|error| StoreError::Query(format!("publish regex generation: {error}")))?;
        crate::durable_sidecar::sync_parent_directory_durable(&path)
            .map_err(|error| StoreError::Query(format!("sync regex generation: {error}")))?;
        // A recreated active scope supersedes its old retirement tombstone.
        crate::durable_sidecar::remove_file_durable_if_exists(&scope_root.join(RETIRED_MARKER))
            .map_err(|error| StoreError::Query(format!("clear regex retirement: {error}")))?;
        let bytes = serde_json::to_vec_pretty(&prepared.pointer)
            .map_err(|error| StoreError::Query(format!("serialize regex pointer: {error}")))?;
        crate::durable_sidecar::atomic_replace_file(&scope_root.join("CURRENT"), |file| {
            file.write_all(&bytes)
        })
        .map_err(|error| StoreError::Query(format!("publish regex pointer: {error}")))?;
        Ok(true)
    }

    /// Apply an exact document delta to the currently selected scope.
    ///
    /// Tantivy commits are atomic for readers. `CURRENT` remains bound to the
    /// previous metadata until the commit has been reopened and validated; in
    /// that short interval the graph/sidecar metadata mismatch makes callers
    /// scan this scope. A crash can therefore make the disposable shard stale,
    /// but can never make search trust a partial update.
    #[cfg(test)]
    pub fn update_scope(
        &self,
        previous: &RegexShardMetadata,
        metadata: RegexShardMetadata,
        upserts: &[RegexShardDocument<'_>],
        deletes: &[RegexDocumentIdentity],
    ) -> Result<(), StoreError> {
        let prepared = self.prepare_update_scope(previous, metadata, upserts, deletes)?;
        if self.publish_prepared(prepared)? {
            Ok(())
        } else {
            Err(StoreError::Query("regex maintenance is busy".into()))
        }
    }

    pub(crate) fn prepare_update_scope(
        &self,
        previous: &RegexShardMetadata,
        metadata: RegexShardMetadata,
        upserts: &[RegexShardDocument<'_>],
        deletes: &[RegexDocumentIdentity],
    ) -> Result<PreparedRegexScope, StoreError> {
        if metadata.schema_version != REGEX_INDEX_SCHEMA_VERSION
            || metadata.tokenizer_fingerprint != REGEX_TOKENIZER_FINGERPRINT
            || metadata.scope_uid != previous.scope_uid
        {
            return Err(StoreError::Query(
                "invalid incremental regex shard metadata".to_string(),
            ));
        }
        let maintenance = self
            .readers
            .maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let staging = tempfile::Builder::new()
            .prefix(".building-")
            .tempdir_in(&self.root)
            .map_err(|error| StoreError::Query(format!("create regex delta staging: {error}")))?;
        for entry in std::fs::read_dir(&path)
            .map_err(|error| StoreError::Query(format!("list regex generation: {error}")))?
        {
            let entry = entry.map_err(|error| {
                StoreError::Query(format!("read regex generation entry: {error}"))
            })?;
            if entry
                .file_type()
                .map_err(|error| StoreError::Query(error.to_string()))?
                .is_file()
                && !entry.file_name().to_string_lossy().ends_with(".lock")
            {
                std::fs::copy(entry.path(), staging.path().join(entry.file_name())).map_err(
                    |error| StoreError::Query(format!("snapshot regex generation: {error}")),
                )?;
            }
        }
        drop(maintenance);
        let path = staging.path().to_path_buf();
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
        for identity in deletes.iter().cloned().chain(
            upserts
                .iter()
                .map(|document| RegexDocumentIdentity::new(document.kind, document.uid)),
        ) {
            writer
                .delete_query(identity.query(fields))
                .map_err(|error| {
                    StoreError::Query(format!("delete regex candidate {}: {error}", identity.uid))
                })?;
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
            tantivy_document.add_text(fields.path, &document.path);
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

        let generation = format!(
            "{}-{}-{}",
            metadata.scope_epoch,
            &metadata.candidate_digest[..metadata.candidate_digest.len().min(16)],
            uuid::Uuid::new_v4()
        );
        Ok(PreparedRegexScope {
            staging,
            pointer: CurrentPointer::new(generation, metadata)?,
        })
    }

    /// Return exact candidate UIDs from a trusted shard. Metadata mismatch is
    /// explicit so callers can widen to a graph scan for this scope.
    pub fn candidate_uids(
        &self,
        expected: &RegexShardMetadata,
        clauses: &[HashSet<String>],
        cap: usize,
    ) -> Result<Option<HashSet<String>>, StoreError> {
        Ok(self
            .candidate_uids_policy(expected, clauses, cap, false, None, None)?
            .and_then(|(uids, cut)| (!cut).then_some(uids)))
    }

    /// Retain a bounded posting prefix with explicit saturation. A ready
    /// shard remains ready when a caller's work allowance is smaller.
    pub fn candidate_uids_bounded(
        &self,
        expected: &RegexShardMetadata,
        clauses: &[HashSet<String>],
        cap: usize,
    ) -> Result<Option<(HashSet<String>, bool)>, StoreError> {
        self.candidate_uids_policy(expected, clauses, cap, true, None, None)
    }

    pub fn candidate_uids_bounded_scoped(
        &self,
        expected: &RegexShardMetadata,
        clauses: &[HashSet<String>],
        cap: usize,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
    ) -> Result<Option<(HashSet<String>, bool)>, StoreError> {
        self.candidate_uids_policy(expected, clauses, cap, true, path_prefix, kinds)
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_uids_policy(
        &self,
        expected: &RegexShardMetadata,
        clauses: &[HashSet<String>],
        cap: usize,
        keep_prefix: bool,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
    ) -> Result<Option<(HashSet<String>, bool)>, StoreError> {
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
        // Eligibility participates in the query before TopDocs applies its
        // retained-prefix budget, so foreign paths/kinds cannot consume it.
        let mut eligible: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, query)];
        if let Some(prefix) = path_prefix {
            let path_query =
                RegexQuery::from_pattern(&format!("{}.*", regex::escape(prefix)), fields.path)
                    .map_err(|error| StoreError::Query(format!("regex path scope: {error}")))?;
            eligible.push((Occur::Must, Box::new(path_query)));
        }
        if let Some(kinds) = kinds {
            let terms: Vec<(Occur, Box<dyn Query>)> = ["Symbol", "Note", "Section", "Frontmatter"]
                .into_iter()
                .filter(|kind| kinds.iter().any(|wanted| wanted.eq_ignore_ascii_case(kind)))
                .map(|kind| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.kind, kind),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            eligible.push((Occur::Must, Box::new(BooleanQuery::new(terms))));
        }
        let query: Box<dyn Query> = Box::new(BooleanQuery::new(eligible));
        // Tantivy's collector reports only the retained hits, not whether more
        // matches existed. Probe one past the caller's budget so saturation is
        // explicit and the caller can conservatively widen this scope to the
        // graph instead of silently dropping regex matches.
        let mut hits = searcher
            .search(
                &*query,
                &TopDocs::with_limit(cap.saturating_add(1)).order_by_score(),
            )
            .map_err(|error| StoreError::Query(format!("query regex shard: {error}")))?;
        let cut = hits.len() > cap;
        if cut && !keep_prefix {
            return Ok(None);
        }
        hits.truncate(cap);
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
        Ok(Some((uids, cut)))
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
    ) -> Result<Option<std::collections::HashMap<RegexDocumentIdentity, String>>, StoreError> {
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
            let kind = extract_text(&document, fields.kind);
            let text_hash = extract_text(&document, fields.text_hash);
            let identity = RegexDocumentIdentity::new(&kind, &uid);
            if uid.is_empty()
                || kind.is_empty()
                || text_hash.is_empty()
                || hashes.insert(identity, text_hash).is_some()
            {
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

    /// Delta for the changed/deleted documents only. Version 6 stores each
    /// document's distinct trigrams, so UID term lookups avoid streaming every
    /// posting in the corpus for a one-note edit.
    pub(crate) fn changed_posting_delta(
        &self,
        scope_uid: &str,
        changed: &[RegexShardDocument<'_>],
        deleted: &[RegexDocumentIdentity],
    ) -> Result<(usize, usize), StoreError> {
        let Some((index, fields, _)) = self.open_current(scope_uid)? else {
            return Ok((changed.iter().map(|doc| doc.trigrams.len()).sum(), 0));
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| StoreError::Query(format!("read changed postings: {e}")))?;
        let searcher = reader.searcher();
        let desired: std::collections::HashMap<_, _> = changed
            .iter()
            .map(|doc| (RegexDocumentIdentity::new(doc.kind, doc.uid), doc.trigrams))
            .collect();
        let mut added = changed.iter().map(|doc| doc.trigrams.len()).sum::<usize>();
        let mut removed = 0;
        for identity in changed
            .iter()
            .map(|doc| RegexDocumentIdentity::new(doc.kind, doc.uid))
            .chain(deleted.iter().cloned())
        {
            let query = identity.query(fields);
            let hits = searcher
                .search(query.as_ref(), &TopDocs::with_limit(2).order_by_score())
                .map_err(|e| StoreError::Query(format!("find changed posting document: {e}")))?;
            if hits.len() > 1 {
                return Err(StoreError::Query(
                    "duplicate live regex document identity".into(),
                ));
            }
            for (_, address) in hits {
                let document: TantivyDocument = searcher.doc(address).map_err(|e| {
                    StoreError::Query(format!("read changed posting document: {e}"))
                })?;
                for trigram in document
                    .get_all(fields.trigram)
                    .filter_map(|value| value.as_str())
                {
                    if desired
                        .get(&identity)
                        .is_some_and(|set| set.contains(trigram))
                    {
                        added -= 1;
                    } else {
                        removed += 1;
                    }
                }
            }
        }
        Ok((added, removed))
    }

    /// Logical live (UID, trigram) set difference, independent of segment
    /// tombstones and reader-cache state. Stream postings instead of retaining
    /// a second corpus-sized set of pairs. Missing first-publication shards
    /// have an empty baseline; unreadable shards report unavailable telemetry.
    pub(crate) fn posting_delta(
        &self,
        scope_uid: &str,
        desired: &[RegexShardDocument<'_>],
    ) -> Result<(usize, usize), StoreError> {
        let desired_count: usize = desired.iter().map(|doc| doc.trigrams.len()).sum();
        let desired: std::collections::HashMap<_, _> = desired
            .iter()
            .map(|doc| (RegexDocumentIdentity::new(doc.kind, doc.uid), doc.trigrams))
            .collect();
        let Some((index, fields, _)) = self.open_current(scope_uid)? else {
            let root = self.scope_root(scope_uid);
            if root
                .try_exists()
                .map_err(|e| StoreError::Query(format!("inspect prior regex shard: {e}")))?
                && !root.join(RETIRED_MARKER).is_file()
            {
                return Err(StoreError::Query(
                    "prior regex shard has no selected generation".to_string(),
                ));
            }
            return Ok((desired_count, 0));
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| StoreError::Query(format!("read regex posting delta: {e}")))?;
        let searcher = reader.searcher();
        let mut previous = 0usize;
        let mut shared = 0usize;
        for (ordinal, segment) in searcher.segment_readers().iter().enumerate() {
            let mut uids = std::collections::HashMap::new();
            let inverted = segment
                .inverted_index(fields.trigram)
                .map_err(|e| StoreError::Query(format!("read regex inverted index: {e}")))?;
            let mut terms = inverted
                .terms()
                .stream()
                .map_err(|e| StoreError::Query(format!("stream regex terms: {e}")))?;
            while let Some((term, info)) = terms.next() {
                let term = std::str::from_utf8(term)
                    .map_err(|e| StoreError::Query(format!("decode regex trigram: {e}")))?;
                let mut postings = inverted
                    .read_postings_from_terminfo(info, IndexRecordOption::Basic)
                    .map_err(|e| StoreError::Query(format!("read regex postings: {e}")))?;
                while postings.doc() != TERMINATED {
                    let id = postings.doc();
                    if !segment.is_deleted(id) {
                        let uid = match uids.entry(id) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                let document: TantivyDocument = searcher
                                    .doc(DocAddress::new(ordinal as u32, id))
                                    .map_err(|e| {
                                        StoreError::Query(format!("decode regex posting UID: {e}"))
                                    })?;
                                entry.insert(RegexDocumentIdentity::new(
                                    &extract_text(&document, fields.kind),
                                    &extract_text(&document, fields.uid),
                                ))
                            }
                        };
                        previous += 1;
                        if desired.get(&*uid).is_some_and(|set| set.contains(term)) {
                            shared += 1;
                        }
                    }
                    postings.advance();
                }
            }
        }
        let added = desired_count.checked_sub(shared).ok_or_else(|| {
            StoreError::Query("duplicate live regex posting identity".to_string())
        })?;
        Ok((added, previous - shared))
    }

    /// Retire a removed scope by durably marking it for reclamation, then
    /// unlinking its selector. Existing readers may finish against immutable
    /// generation files; cleanup is a separate retention operation performed
    /// by [`Self::garbage_collect`].
    ///
    /// The [`RETIRED_MARKER`] tombstone is written *before* `CURRENT` is
    /// removed, and that order is load-bearing (nw-374). An absent `CURRENT`
    /// is not, by itself, proof that a scope was retired — `replace_scope`'s
    /// own crash window (rename-then-select) produces that exact state for a
    /// scope's first publication, and garbage collection must not confuse the
    /// two. Writing the tombstone first means any crash after this call
    /// begins can only ever resolve toward "fully retired" (tombstone
    /// present, so GC reclaims the whole scope) and never toward
    /// resurrecting a scope that was deliberately dropped from the graph. The
    /// converse order would let a crash between removing `CURRENT` and
    /// writing the tombstone leave a retired scope looking exactly like a
    /// first-publication crash, which garbage collection would then "adopt"
    /// back to life.
    ///
    /// A caller that receives an error from this function has an
    /// incompletely retired scope on disk and MUST retry until it succeeds;
    /// do not call this for a scope that should remain live.
    #[cfg(test)]
    pub fn retire_scope(&self, scope_uid: &str) -> Result<bool, StoreError> {
        self.try_retire_scope(scope_uid)?
            .ok_or_else(|| StoreError::Query("regex maintenance is busy".into()))
    }

    pub(crate) fn try_retire_scope(&self, scope_uid: &str) -> Result<Option<bool>, StoreError> {
        let _maintenance = match self.readers.maintenance.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        };
        self.readers.clear();
        let scope_root = self.scope_root(scope_uid);
        if !scope_root.exists() {
            return Ok(Some(false));
        }
        let tombstone = scope_root.join(RETIRED_MARKER);
        crate::durable_sidecar::atomic_replace_file(&tombstone, |file| {
            file.write_all(RETIRED_MARKER.as_bytes())
        })
        .map_err(|error| {
            StoreError::Query(format!("retire regex scope {scope_uid}: mark {error}"))
        })?;
        let current = scope_root.join("CURRENT");
        let selector_removed = crate::durable_sidecar::remove_file_durable_if_exists(&current)
            .map_err(|error| {
                StoreError::Query(format!("retire regex scope {scope_uid}: {error}"))
            })?;
        Ok(Some(selector_removed))
    }

    /// Remove unselected generations and durably-tombstoned retired scopes.
    ///
    /// This is deliberately separate from publication: a reader that still
    /// holds an old generation may delay deletion on Windows. The selected
    /// generation is never removed, and a corrupt selector fails closed
    /// rather than guessing which generation is live. A later refresh retries
    /// cleanup, so transient descriptor pressure cannot lose the shard.
    ///
    /// A scope whose `CURRENT` is absent is deleted ONLY when it also carries
    /// the [`RETIRED_MARKER`] tombstone (nw-374). Without the tombstone, an
    /// absent `CURRENT` means `replace_scope` crashed between renaming a
    /// generation into place and durably selecting it — a live scope's first
    /// publication in progress, not garbage. That case is recovered by
    /// adopting the sole leftover generation as `CURRENT` (see
    /// [`Self::recover_missing_current`]) rather than discarding it, closing
    /// the crash window by construction instead of by luck of timing.
    pub fn garbage_collect(&self) -> Result<RegexGarbageCollectionReport, StoreError> {
        let _maintenance = self
            .readers
            .maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            if scope_root.join(RETIRED_MARKER).exists() {
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
            let current_path = scope_root.join("CURRENT");
            let bytes = match std::fs::read(&current_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match self.recover_missing_current(&scope_root, &entry_hash) {
                        Ok(MissingCurrentRecovery::Adopted) => report.adopted += 1,
                        Ok(MissingCurrentRecovery::Empty) => {
                            match std::fs::remove_dir_all(&scope_root) {
                                Ok(()) => report.removed += 1,
                                Err(error) => report.failures.push(RegexScopeIssue {
                                    scope_hash: entry_hash.clone(),
                                    error: format!(
                                        "remove empty regex shard {}: {error}",
                                        scope_root.display()
                                    ),
                                }),
                            }
                        }
                        Err(error) => report.failures.push(RegexScopeIssue {
                            scope_hash: entry_hash.clone(),
                            error: error.to_string(),
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

    /// Recover a scope whose `CURRENT` is absent and which carries no
    /// [`RETIRED_MARKER`] tombstone.
    ///
    /// `update_scope` requires an existing `CURRENT` to run at all, so this
    /// state can only arise from `replace_scope`'s crash window on a scope's
    /// FIRST publication: the new generation was renamed into place (and
    /// durably synced) but the process died before `CURRENT` was written to
    /// select it. Exactly one generation directory therefore exists. This
    /// adopts it — verifying it opens, that its embedded metadata is
    /// self-consistent, and that it belongs to this scope hash — and durably
    /// writes `CURRENT` to select it, so garbage collection heals the crash
    /// instead of discarding a complete shard.
    ///
    /// More than one leftover generation, or one that fails validation, is
    /// not a shape this window produces; garbage collection fails closed
    /// (reports a failure, deletes nothing) rather than guessing.
    fn recover_missing_current(
        &self,
        scope_root: &Path,
        entry_hash: &str,
    ) -> Result<MissingCurrentRecovery, StoreError> {
        let generations_dir = scope_root.join("generations");
        let mut candidates: Vec<PathBuf> = match std::fs::read_dir(&generations_dir) {
            Ok(entries) => {
                let mut paths = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        StoreError::Query(format!(
                            "read regex generation for recovery {}: {error}",
                            generations_dir.display()
                        ))
                    })?;
                    let is_dir = entry.file_type().map_err(|error| {
                        StoreError::Query(format!("inspect regex generation for recovery: {error}"))
                    })?;
                    if is_dir.is_dir() {
                        paths.push(entry.path());
                    }
                }
                paths
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(StoreError::Query(format!(
                    "list regex generations for recovery {}: {error}",
                    generations_dir.display()
                )));
            }
        };
        if candidates.is_empty() {
            return Ok(MissingCurrentRecovery::Empty);
        }
        if candidates.len() > 1 {
            return Err(StoreError::Query(format!(
                "refusing to recover regex shard {}: {} generations with no CURRENT and no retirement marker",
                scope_root.display(),
                candidates.len()
            )));
        }
        let path = candidates.remove(0);
        let index = Index::open_in_dir(&path).map_err(|error| {
            StoreError::Query(format!(
                "open leftover regex generation {} for recovery: {error}",
                path.display()
            ))
        })?;
        let fields = inspect_fields(&index.schema())?;
        let (metadata, count) = read_metadata(&index, fields)?;
        if count != metadata.candidate_count {
            return Err(StoreError::Query(format!(
                "refusing to recover regex shard {}: metadata/content mismatch",
                path.display()
            )));
        }
        if scope_hash(&metadata.scope_uid) != entry_hash {
            return Err(StoreError::Query(format!(
                "refusing to recover regex shard {} after scope-hash collision for {}",
                path.display(),
                metadata.scope_uid
            )));
        }
        // A crash-orphaned generation from a schema this binary no longer
        // builds must not be silently adopted just because it is otherwise
        // self-consistent: that would resurrect exactly the kind of stale
        // artifact the tombstone work above exists to prevent, only via the
        // adoption path instead of the deletion one. `refresh_regex_v3`
        // already treats a schema/fingerprint mismatch as reason to rebuild
        // (see its `compatible` check), so an old-schema orphan is not lost —
        // it is left for a human or a future run to reconcile, not silently
        // reused.
        if metadata.schema_version != REGEX_INDEX_SCHEMA_VERSION
            || metadata.tokenizer_fingerprint != REGEX_TOKENIZER_FINGERPRINT
        {
            return Err(StoreError::Query(format!(
                "refusing to recover regex shard {}: schema {}/{:?} does not match this binary's {}/{:?}",
                path.display(),
                metadata.schema_version,
                metadata.tokenizer_fingerprint,
                REGEX_INDEX_SCHEMA_VERSION,
                REGEX_TOKENIZER_FINGERPRINT
            )));
        }
        let generation_name = path
            .file_name()
            .ok_or_else(|| {
                StoreError::Query(format!(
                    "leftover regex generation {} has no file name",
                    path.display()
                ))
            })?
            .to_string_lossy()
            .into_owned();
        let pointer = CurrentPointer::new(generation_name, metadata)?;
        let pointer_path = scope_root.join("CURRENT");
        let bytes = serde_json::to_vec_pretty(&pointer)
            .map_err(|error| StoreError::Query(format!("serialize regex pointer: {error}")))?;
        crate::durable_sidecar::atomic_replace_file(&pointer_path, |file| file.write_all(&bytes))
            .map_err(|error| StoreError::Query(format!("adopt recovered regex pointer: {error}")))?;
        Ok(MissingCurrentRecovery::Adopted)
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

/// Read the staged `meta.json` once. `NotFound` is not a refresh failure:
/// the file can still be invisible immediately after commit (nw-744).
fn snapshot_staged_meta(meta_path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match read_staged_meta_file(meta_path) {
        Ok(bytes) if bytes.is_empty() => Err(StoreError::Query(
            "staged regex shard meta.json was empty after commit".to_string(),
        )),
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::Query(format!(
            "read staged regex shard meta.json {}: {error}",
            meta_path.display()
        ))),
    }
}

fn read_staged_meta_file(meta_path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(test)]
    if let Some(miss) = take_post_commit_meta_miss(meta_path) {
        return miss;
    }
    std::fs::read(meta_path)
}

/// Open the staged shard. `FileDoesNotExist(meta.json)` retries the open.
/// Other open failures, including a `meta.json` whose segments are gone, fail
/// closed so a bad generation is not published.
fn reopen_staged_regex_shard(
    path: &Path,
    mut committed_meta: Option<Vec<u8>>,
) -> Result<Index, StoreError> {
    rewrite_staged_meta_snapshot(path, committed_meta.as_deref())?;
    match Index::open_in_dir(path) {
        Ok(index) => Ok(index),
        Err(error) if staged_meta_missing(&error) => {
            if committed_meta.is_none() {
                committed_meta = snapshot_staged_meta(&path.join("meta.json"))?;
            }
            rewrite_staged_meta_snapshot(path, committed_meta.as_deref())?;
            Index::open_in_dir(path)
                .map_err(|retry| StoreError::Query(format!("reopen staged regex shard: {retry}")))
        }
        Err(error) => Err(StoreError::Query(format!(
            "reopen staged regex shard: {error}"
        ))),
    }
}

fn staged_meta_missing(error: &tantivy::TantivyError) -> bool {
    let text = error.to_string();
    text.contains("FileDoesNotExist") && text.contains("meta.json")
}

/// Write `committed_meta` only when a read already produced those bytes and
/// `meta.json` is still absent. An existing file is left untouched.
fn rewrite_staged_meta_snapshot(
    path: &Path,
    committed_meta: Option<&[u8]>,
) -> Result<(), StoreError> {
    let Some(committed_meta) = committed_meta else {
        return Ok(());
    };
    if committed_meta.is_empty() {
        return Err(StoreError::Query(
            "staged regex shard meta.json was empty after commit".to_string(),
        ));
    }
    let meta_path = path.join("meta.json");
    if !meta_path_absent(&meta_path)? {
        return Ok(());
    }
    crate::durable_sidecar::atomic_replace_file(&meta_path, |file| file.write_all(committed_meta))
        .map_err(|error| {
            StoreError::Query(format!(
                "restore staged regex shard meta.json {}: {error}",
                meta_path.display()
            ))
        })
}

fn meta_path_absent(meta_path: &Path) -> Result<bool, StoreError> {
    match meta_path.try_exists() {
        Ok(exists) => Ok(!exists),
        Err(error) => Err(StoreError::Query(format!(
            "stat staged regex shard meta.json {}: {error}",
            meta_path.display()
        ))),
    }
}

#[cfg(test)]
thread_local! {
    static MISS_POST_COMMIT_META_READ: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static STAGED_META_FAULT_FIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One-shot nw-744 seam. The post-commit `std::fs::read` of `meta.json` is the
/// call that observes the absence. The committed bytes are put back after that
/// read so the following open still sees the segment set.
#[cfg(test)]
fn take_post_commit_meta_miss(meta_path: &Path) -> Option<std::io::Result<Vec<u8>>> {
    let armed = MISS_POST_COMMIT_META_READ.with(|cell| cell.replace(false));
    if !armed {
        return None;
    }
    let preserved = std::fs::read(meta_path).unwrap_or_else(|error| {
        panic!(
            "stage meta.json before the post-commit miss {}: {error}",
            meta_path.display()
        )
    });
    std::fs::remove_file(meta_path)
        .unwrap_or_else(|error| panic!("remove staged meta.json {}: {error}", meta_path.display()));
    let missed = std::fs::read(meta_path);
    assert!(
        missed
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "post-commit read did not observe a missing meta.json"
    );
    std::fs::write(meta_path, preserved).unwrap_or_else(|error| {
        panic!("restore staged meta.json {}: {error}", meta_path.display())
    });
    STAGED_META_FAULT_FIRED.with(|cell| cell.set(true));
    Some(missed)
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

    fn preservable_fixture(root: &Path) -> crate::PublicationIdentity {
        let identity = crate::PublicationIdentity::new_brain();
        let index = RegexIndex::new(root);
        for epoch in [1_u64, 2] {
            let digest = blake3::hash(&epoch.to_le_bytes()).to_hex().to_string();
            let mut metadata = metadata(epoch, 1, &digest);
            metadata.brain_uuid = identity.brain_uuid.clone();
            metadata.publication_uuid = identity.publication_uuid.clone();
            let trigrams = HashSet::from(["abc".into()]);
            index
                .replace_scope(
                    metadata,
                    &[RegexShardDocument {
                        uid: "sym:fixture",
                        kind: "Symbol",
                        path: "fixture.rs".into(),
                        text_hash: "hash",
                        trigrams: &trigrams,
                    }],
                )
                .unwrap();
        }
        identity
    }

    fn layout_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fn visit(
            root: &Path,
            dir: &Path,
            values: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &entry.path(), values);
                } else if entry.file_type().unwrap().is_file() {
                    values.insert(
                        entry.path().strip_prefix(root).unwrap().into(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        let mut values = std::collections::BTreeMap::new();
        visit(root, root, &mut values);
        values
    }

    #[test]
    fn live_regex_recognizer_checks_every_generation_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let identity = preservable_fixture(dir.path());
        let before = layout_bytes(dir.path());
        assert!(is_preservable_live_layout(dir.path(), &identity, 2).unwrap());
        assert_eq!(
            layout_bytes(dir.path()),
            before,
            "recognition must not create locks or repair descriptors"
        );
        let index = RegexIndex::new(dir.path());
        let pointer = index.current_pointer("repo:test").unwrap().unwrap();
        let generations = index.scope_root("repo:test").join("generations");
        let old = std::fs::read_dir(&generations)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() != pointer.generation.as_str())
            .unwrap()
            .path();
        std::fs::write(old.join("personal.txt"), b"irreplaceable").unwrap();
        let with_personal = layout_bytes(dir.path());
        assert!(!is_preservable_live_layout(dir.path(), &identity, 2).unwrap());
        assert_eq!(
            layout_bytes(dir.path()),
            with_personal,
            "unknown files must remain unchanged"
        );
    }

    #[test]
    fn live_regex_recognizer_refuses_foreign_stale_and_forged_selectors() {
        let dir = tempfile::tempdir().unwrap();
        let identity = preservable_fixture(dir.path());
        assert!(
            !is_preservable_live_layout(dir.path(), &crate::PublicationIdentity::new_brain(), 2)
                .unwrap()
        );
        assert!(!is_preservable_live_layout(dir.path(), &identity, 1).unwrap());
        let index = RegexIndex::new(dir.path());
        let path = index.scope_root("repo:test").join("CURRENT");
        let mut pointer: CurrentPointer =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        pointer.metadata.candidate_count += 1;
        pointer.checksum = pointer.expected_checksum().unwrap();
        std::fs::write(&path, serde_json::to_vec(&pointer).unwrap()).unwrap();
        let before = layout_bytes(dir.path());
        assert!(!is_preservable_live_layout(dir.path(), &identity, 2).unwrap());
        assert_eq!(layout_bytes(dir.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn live_regex_recognizer_refuses_symlinks_and_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let identity = preservable_fixture(dir.path());
        let outside = tempfile::tempdir().unwrap();
        let personal = outside.path().join("personal");
        std::fs::write(&personal, b"mine").unwrap();
        std::os::unix::fs::symlink(&personal, dir.path().join("linked")).unwrap();
        assert!(!is_preservable_live_layout(dir.path(), &identity, 2).unwrap());
        std::fs::remove_file(dir.path().join("linked")).unwrap();
        let current = RegexIndex::new(dir.path())
            .scope_root("repo:test")
            .join("CURRENT");
        std::fs::hard_link(&current, outside.path().join("hardlink")).unwrap();
        assert!(!is_preservable_live_layout(dir.path(), &identity, 2).unwrap());
        assert_eq!(std::fs::read(personal).unwrap(), b"mine");
    }

    /// nw-744: the post-commit read of staged `meta.json` can be `NotFound`.
    /// That miss must not fail publication. One shot — the fault makes that
    /// read the call that observes the missing file.
    #[test]
    fn staged_reopen_restores_missing_meta_json() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["abc".into()]);
        let document = RegexShardDocument {
            path: "src/lib.rs".into(),
            uid: "one",
            kind: "Symbol",
            text_hash: "hash",
            trigrams: &trigrams,
        };
        let published = metadata(1, 1, "digest-one");
        struct ClearStagedMetaFault;
        impl Drop for ClearStagedMetaFault {
            fn drop(&mut self) {
                MISS_POST_COMMIT_META_READ.with(|cell| cell.set(false));
                STAGED_META_FAULT_FIRED.with(|cell| cell.set(false));
            }
        }
        let _clear = ClearStagedMetaFault;
        STAGED_META_FAULT_FIRED.with(|cell| cell.set(false));
        MISS_POST_COMMIT_META_READ.with(|cell| cell.set(true));

        index
            .replace_scope(published.clone(), std::slice::from_ref(&document))
            .unwrap();

        assert!(
            STAGED_META_FAULT_FIRED.with(|cell| cell.get()),
            "the post-commit read did not observe a missing staged meta.json"
        );
        let generations = index.scope_root("repo:test").join("generations");
        let published_meta = std::fs::read_dir(&generations)
            .unwrap()
            .map(|entry| entry.unwrap().path().join("meta.json"))
            .find(|path| path.is_file())
            .expect("published generation is missing meta.json");
        assert!(!std::fs::read(&published_meta).unwrap().is_empty());
        assert_eq!(
            index
                .candidate_uids(&published, std::slice::from_ref(&trigrams), 10)
                .unwrap(),
            Some(HashSet::from(["one".to_string()]))
        );
    }

    #[test]
    fn posting_deltas_ignore_dead_documents_and_reader_cache() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let old = HashSet::from(["abc".into(), "bcd".into()]);
        let new = HashSet::from(["bcd".into(), "cde".into(), "def".into()]);
        let first = RegexShardDocument {
            path: "fixture.rs".into(),
            uid: "one",
            kind: "Symbol",
            text_hash: "old",
            trigrams: &old,
        };
        assert_eq!(
            index
                .posting_delta("repo:test", std::slice::from_ref(&first))
                .unwrap(),
            (2, 0)
        );
        let initial = metadata(1, 1, "one");
        index
            .replace_scope(initial.clone(), std::slice::from_ref(&first))
            .unwrap();
        // Warm the shared reader pool before in-place segment updates.
        index
            .candidate_uids(&initial, std::slice::from_ref(&old), 10)
            .unwrap();
        assert_eq!(index.posting_delta("repo:test", &[first]).unwrap(), (0, 0));
        let edited = RegexShardDocument {
            path: "fixture.rs".into(),
            uid: "one",
            kind: "Symbol",
            text_hash: "new",
            trigrams: &new,
        };
        assert_eq!(
            index
                .posting_delta("repo:test", std::slice::from_ref(&edited))
                .unwrap(),
            (2, 1)
        );
        let updated = metadata(2, 1, "two");
        index
            .update_scope(
                &initial,
                updated.clone(),
                std::slice::from_ref(&edited),
                &[],
            )
            .unwrap();
        assert_eq!(
            index
                .posting_delta("repo:test", std::slice::from_ref(&edited))
                .unwrap(),
            (0, 0)
        );
        index
            .replace_scope(metadata(3, 1, "two"), &[edited])
            .unwrap();
        let renamed = RegexShardDocument {
            path: "fixture.rs".into(),
            uid: "renamed",
            kind: "Symbol",
            text_hash: "new",
            trigrams: &new,
        };
        assert_eq!(
            index
                .posting_delta("repo:test", std::slice::from_ref(&renamed))
                .unwrap(),
            (3, 3)
        );
        index
            .replace_scope(metadata(4, 1, "three"), &[renamed])
            .unwrap();
        assert_eq!(index.posting_delta("repo:test", &[]).unwrap(), (0, 3));
        index.retire_scope("repo:test").unwrap();
        assert_eq!(index.posting_delta("repo:test", &[]).unwrap(), (0, 0));
    }

    #[test]
    fn exact_delta_selects_immutable_sibling_without_retokenizing_the_scope() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let first_trigrams = HashSet::from(["alp".to_string(), "lph".to_string()]);
        let first = RegexShardDocument {
            path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
            uid: "sym:two",
            kind: "Symbol",
            text_hash: "hash-two",
            trigrams: &second_trigrams,
        };
        let updated = metadata(2, 1, "digest-two");
        let prepared = index
            .prepare_update_scope(
                &initial,
                updated.clone(),
                &[second],
                &[RegexDocumentIdentity::new("Symbol", "sym:one")],
            )
            .unwrap();
        assert_eq!(
            index
                .current_pointer("repo:test")
                .unwrap()
                .unwrap()
                .generation,
            generation
        );
        assert_eq!(
            index.document_hashes("repo:test").unwrap().unwrap(),
            std::collections::HashMap::from([(
                RegexDocumentIdentity::new("Symbol", "sym:one"),
                "hash-one".to_string()
            )]),
            "preparation must not mutate the served generation"
        );
        index.garbage_collect().unwrap();
        assert!(
            index.publish_prepared(prepared).unwrap(),
            "GC must not reclaim private preparation"
        );

        let pointer = index.current_pointer("repo:test").unwrap().unwrap();
        assert_ne!(
            pointer.generation, generation,
            "delta publication must select an immutable sibling"
        );
        assert_eq!(pointer.metadata, updated);
        assert_eq!(
            index.document_hashes("repo:test").unwrap().unwrap(),
            std::collections::HashMap::from([(
                RegexDocumentIdentity::new("Symbol", "sym:two"),
                "hash-two".to_string()
            )])
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
    fn bounded_candidate_prefix_reports_saturation_and_exact_completion() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let documents: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|uid| RegexShardDocument {
                path: "fixture.rs".into(),
                uid,
                kind: "Symbol",
                text_hash: uid,
                trigrams: &trigrams,
            })
            .collect();
        let published = metadata(1, documents.len(), "bounded-three");
        index.replace_scope(published.clone(), &documents).unwrap();
        let clauses = [HashSet::from(["alp".to_string()])];
        let (uids, cut) = index
            .candidate_uids_bounded(&published, &clauses, 2)
            .unwrap()
            .unwrap();
        assert_eq!(uids.len(), 2);
        assert!(cut);
        let (uids, cut) = index
            .candidate_uids_bounded(&published, &clauses, 3)
            .unwrap()
            .unwrap();
        assert_eq!(uids.len(), 3);
        assert!(!cut);
        let (uids, cut) = index
            .candidate_uids_bounded(&published, &clauses, 0)
            .unwrap()
            .unwrap();
        assert!(uids.is_empty());
        assert!(cut);
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
                path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
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
                path: "fixture.rs".into(),
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
            path: "fixture.rs".into(),
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

    /// nw-374: a crash between `replace_scope` renaming its generation into
    /// place and durably writing `CURRENT` must not cost the shard. Simulated
    /// via the `Persist` fault, which fails exactly the `CURRENT` write after
    /// the generation directory has already been renamed and synced.
    #[test]
    fn first_publication_crash_before_current_is_adopted_not_destroyed() {
        use crate::durable_sidecar::{TestFault, with_test_fault};

        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            path: "fixture.rs".into(),
            uid: "sym:one",
            kind: "Symbol",
            text_hash: "hash-one",
            trigrams: &trigrams,
        };
        let meta = metadata(1, 1, "digest-one");

        let error = with_test_fault(TestFault::Persist, || {
            index.replace_scope(meta.clone(), std::slice::from_ref(&document))
        })
        .unwrap_err();
        assert!(error.to_string().contains("publish regex pointer"));

        // This scope has never been selected: no prior shard exists to fall
        // back to, and CURRENT was never written.
        assert!(index.metadata("repo:test").unwrap().is_none());
        assert!(!index.scope_root("repo:test").join("CURRENT").exists());
        let generations = index.scope_root("repo:test").join("generations");
        assert_eq!(
            std::fs::read_dir(&generations).unwrap().count(),
            1,
            "the complete generation from the crashed publish must still be on disk"
        );

        let report = index.garbage_collect().unwrap();
        assert_eq!(
            report.adopted, 1,
            "the leftover generation must be adopted, not deleted"
        );
        assert_eq!(report.removed, 0);
        assert!(report.failures.is_empty());

        // Adoption closes the window completely: the shard is selected and
        // searchable without a rebuild, and it is the SAME generation.
        assert_eq!(index.metadata("repo:test").unwrap(), Some(meta.clone()));
        assert_eq!(
            index
                .candidate_uids(&meta, &[HashSet::from(["alp".to_string()])], 10)
                .unwrap()
                .unwrap(),
            HashSet::from(["sym:one".to_string()])
        );
        assert_eq!(
            std::fs::read_dir(&generations).unwrap().count(),
            1,
            "no rebuild occurred; the crashed generation was reused"
        );
    }

    /// nw-374: an absent `CURRENT` with MORE THAN ONE leftover generation and
    /// no retirement tombstone is not a shape either `replace_scope`'s crash
    /// window or `retire_scope` produces. Garbage collection must fail closed
    /// (report a failure, delete nothing) rather than guess which generation,
    /// if any, is safe to select.
    #[test]
    fn missing_current_with_multiple_leftover_generations_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let trigrams = HashSet::from(["alp".to_string()]);
        let document = RegexShardDocument {
            path: "fixture.rs".into(),
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
        // Hand-craft the otherwise-unreachable state: two generations, no
        // CURRENT, no tombstone. Neither real code path produces this; it
        // stands in for on-disk corruption a real crash cannot cause.
        std::fs::remove_file(index.scope_root("repo:test").join("CURRENT")).unwrap();

        let generations = index.scope_root("repo:test").join("generations");
        assert_eq!(std::fs::read_dir(&generations).unwrap().count(), 2);
        let report = index.garbage_collect().unwrap();
        assert_eq!(report.adopted, 0);
        assert_eq!(report.removed, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            std::fs::read_dir(&generations).unwrap().count(),
            2,
            "an ambiguous state must not be guessed at, in either direction"
        );
    }

    /// nw-374: a scope directory with no generations at all and no `CURRENT`
    /// (e.g. `replace_scope` crashed before ever renaming a generation into
    /// place) holds nothing worth recovering and is safe to reclaim.
    #[test]
    fn missing_current_with_no_generations_is_reclaimed_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let scope_root = index.scope_root("repo:test");
        std::fs::create_dir_all(scope_root.join("generations")).unwrap();

        let report = index.garbage_collect().unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.adopted, 0);
        assert!(!scope_root.exists());
    }

    /// nw-374 review fix 3: a crash-orphaned generation built under a schema
    /// or tokenizer this binary no longer produces must not be silently
    /// adopted just because it is otherwise self-consistent (opens, count
    /// matches, scope hash matches). Adopting it would resurrect a stale
    /// artifact after an upgrade — the exact class of thing the tombstone
    /// work exists to prevent, reached through recovery instead of deletion.
    /// `replace_scope` itself refuses to BUILD a mismatched generation, so
    /// this is hand-built to simulate one left behind by an older binary.
    #[test]
    fn missing_current_with_mismatched_schema_is_not_adopted() {
        let temp = tempfile::tempdir().unwrap();
        let index = RegexIndex::new(temp.path());
        let scope_uid = "repo:test";
        let scope_root = index.scope_root(scope_uid);
        let generations = scope_root.join("generations");
        std::fs::create_dir_all(&generations).unwrap();

        let mut stale_metadata = metadata(1, 0, "digest-stale");
        stale_metadata.schema_version = REGEX_INDEX_SCHEMA_VERSION - 1;
        let generation_path = generations.join("stale-generation");
        std::fs::create_dir_all(&generation_path).unwrap();
        let (schema, fields) = build_schema();
        let tantivy_index = Index::create_in_dir(&generation_path, schema).unwrap();
        let mut writer = tantivy_index.writer(15_000_000).unwrap();
        let encoded_metadata = serde_json::to_string(&stale_metadata).unwrap();
        writer
            .add_document(doc!(
                fields.uid => format!("meta:{scope_uid}"),
                fields.kind => METADATA_KIND.to_string(),
                fields.metadata => encoded_metadata,
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(tantivy_index);

        // No CURRENT, no tombstone: the same shape a first-publication crash
        // leaves, except this generation is from a schema the binary no
        // longer builds.
        let report = index.garbage_collect().unwrap();
        assert_eq!(report.adopted, 0, "{report:?}");
        assert_eq!(report.removed, 0, "{report:?}");
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert!(
            report.failures[0].error.contains("schema"),
            "the refusal must explain why: {:?}",
            report.failures[0]
        );
        assert!(
            !scope_root.join("CURRENT").exists(),
            "a mismatched-schema orphan must not be adopted"
        );
        assert!(
            generation_path.exists(),
            "a mismatched-schema orphan must not be guessed at and deleted either"
        );
    }
}

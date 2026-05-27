//! Tantivy BM25 index over the brain's text content.
//!
//! Lives alongside the LadybugDB graph and indexes everything searchable:
//! note titles, heading text, section bodies, tag names. Each Tantivy
//! document carries the graph node's UID + kind so search hits can be
//! resolved back into graph operations (fetch the note, run PPR from it,
//! etc.).
//!
//! Thread model: Tantivy's `IndexWriter` is exclusive (single-writer
//! per index). Read paths use cheap `Searcher` handles. The struct
//! wraps the writer in a `Mutex` so callers can share a `TantivyIndex`
//! across threads — the watcher and MCP server can both hold one.
//!
//! Persistence: the index lives at `<db_path>.tantivy/`. Tantivy handles
//! its own mmap/segment lifecycle; we just point it at the directory.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term, doc};
use thiserror::Error;

use crate::db::GraphStore;
use crate::error::StoreError;

/// Single search result from the BM25 index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub vault_uid: String,
    pub score: f32,
}

#[derive(Debug, Error)]
pub enum TantivyError {
    #[error("tantivy: {0}")]
    Tantivy(String),
    #[error("io: {0}")]
    Io(String),
    #[error("writer unavailable: index opened in read-only mode")]
    WriterUnavailable,
}

impl From<tantivy::TantivyError> for TantivyError {
    fn from(e: tantivy::TantivyError) -> Self {
        TantivyError::Tantivy(e.to_string())
    }
}

impl From<std::io::Error> for TantivyError {
    fn from(e: std::io::Error) -> Self {
        TantivyError::Io(e.to_string())
    }
}

impl From<TantivyError> for StoreError {
    fn from(e: TantivyError) -> Self {
        StoreError::Query(e.to_string())
    }
}

/// BM25 index over the brain's text content.
///
/// Two constructors:
/// - `open_or_create` — opens both a reader and writer. Use this in
///   processes that need to write (the brain watcher, `brain add`,
///   `brain reindex-search`).
/// - `open_reader_only` — opens only a reader. Use this in processes
///   that only need to search (CLI search, MCP server, web UI). This
///   avoids contending for the writer lock with a running watcher.
///
/// Write methods (`reindex_from_store`, `update_note`, `remove_note`)
/// return `TantivyError::WriterUnavailable` when called on a
/// reader-only instance.
pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: Option<Mutex<tantivy::IndexWriter>>,
    fields: Fields,
}

/// Field handles bundled together so we don't look them up by name on
/// every operation.
struct Fields {
    uid: Field,
    kind: Field,
    title: Field,
    body: Field,
    vault_uid: Field,
    note_uid: Field, // for batch deletion of all docs belonging to a note
}

impl TantivyIndex {
    /// Open the on-disk index at `path` or create it if missing. The
    /// directory is created if needed. Existing indices keep their
    /// segments; callers wanting a fresh start should remove the
    /// directory first.
    pub fn open_or_create(path: &Path) -> Result<Self, TantivyError> {
        std::fs::create_dir_all(path)?;
        let schema = build_schema();

        // Try open first; fall back to creation.
        let index = match Index::open_in_dir(path) {
            Ok(idx) => idx,
            Err(_) => Index::create_in_dir(path, schema.clone())?,
        };

        // ~50 MB write buffer — sufficient for vaults up to ~50K
        // documents per the architecture doc's memory budget.
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let fields = lookup_fields(&schema);

        Ok(Self {
            index,
            reader,
            writer: Some(Mutex::new(writer)),
            fields,
        })
    }

    /// Open the on-disk index at `path` in read-only mode. No writer is
    /// acquired, so this will succeed even when another process (e.g. the
    /// brain watcher) holds the writer lock. Write operations on the
    /// returned instance will return `TantivyError::WriterUnavailable`.
    ///
    /// Returns `Err` if the index directory does not exist or is corrupt.
    pub fn open_reader_only(path: &Path) -> Result<Self, TantivyError> {
        let index = Index::open_in_dir(path)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let schema = index.schema();
        let fields = lookup_fields(&schema);

        Ok(Self {
            index,
            reader,
            writer: None,
            fields,
        })
    }

    /// Drop every document and rebuild from the current state of `store`.
    /// Use after a fresh `index_markdown_directory` or as a manual escape
    /// hatch (`nestweaver brain reindex-search`).
    pub fn reindex_from_store(&self, store: &GraphStore) -> Result<usize, TantivyError> {
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex.lock().unwrap();
        writer.delete_all_documents()?;
        let count = self.write_full_corpus(&mut writer, store)?;
        writer.commit()?;
        // Manually reload the reader so subsequent searches see the new
        // segments without waiting for the OnCommitWithDelay tick.
        self.reader.reload()?;
        Ok(count)
    }

    /// Per-note incremental update. Drops every Tantivy doc tagged with
    /// `note_uid` (the note itself + all its headings + sections) and
    /// re-indexes the supplied fresh data. Called by the file watcher
    /// after re-parsing a saved file.
    #[allow(clippy::too_many_arguments)]
    pub fn update_note(
        &self,
        note_uid: &str,
        title: &str,
        vault_uid: &str,
        body_chunks: &[String],
        headings: &[(String, String)],
        sections: &[(String, String)],
        tags: &[String],
    ) -> Result<(), TantivyError> {
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex.lock().unwrap();
        // Remove all docs with this note_uid first.
        writer.delete_term(Term::from_field_text(self.fields.note_uid, note_uid));
        // Add fresh.
        // 1. The note itself (body = concatenated chunks for whole-note BM25).
        let combined_body = body_chunks.join("\n\n");
        writer.add_document(doc!(
            self.fields.uid => note_uid.to_string(),
            self.fields.kind => "note".to_string(),
            self.fields.title => title.to_string(),
            self.fields.body => combined_body,
            self.fields.vault_uid => vault_uid.to_string(),
            self.fields.note_uid => note_uid.to_string(),
        ))?;
        // 2. Per-heading docs.
        for (h_uid, h_text) in headings {
            writer.add_document(doc!(
                self.fields.uid => h_uid.to_string(),
                self.fields.kind => "heading".to_string(),
                self.fields.title => h_text.to_string(),
                self.fields.body => h_text.to_string(),
                self.fields.vault_uid => vault_uid.to_string(),
                self.fields.note_uid => note_uid.to_string(),
            ))?;
        }
        // 3. Per-section docs.
        for (s_uid, s_body) in sections {
            writer.add_document(doc!(
                self.fields.uid => s_uid.to_string(),
                self.fields.kind => "section".to_string(),
                self.fields.title => String::new(),
                self.fields.body => s_body.to_string(),
                self.fields.vault_uid => vault_uid.to_string(),
                self.fields.note_uid => note_uid.to_string(),
            ))?;
        }
        // 4. Tags are indexed once globally (in reindex_from_store), not
        //    per note — multiple notes share tags. Skip here.
        let _ = tags;

        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Drop every Tantivy doc belonging to `note_uid`. Called by the
    /// watcher on file delete.
    pub fn remove_note(&self, note_uid: &str) -> Result<(), TantivyError> {
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex.lock().unwrap();
        writer.delete_term(Term::from_field_text(self.fields.note_uid, note_uid));
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// BM25 search across title + body fields. Returns up to `limit`
    /// hits ranked by Tantivy's default BM25 scoring.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, TantivyError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let mut parser =
            QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        parser.set_field_boost(self.fields.title, 3.0);
        let parsed = match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => {
                // Fall back to a "lenient" parse that escapes special
                // chars by quoting — handles common user input like
                // `auth/v2` or `path:foo` that breaks the parser.
                let escaped = escape_query(query);
                parser
                    .parse_query(&escaped)
                    .map_err(|e| TantivyError::Tantivy(e.to_string()))?
            }
        };
        let top = searcher.search(&parsed, &TopDocs::with_limit(limit).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, address) in top {
            let doc: TantivyDocument = searcher.doc(address)?;
            hits.push(SearchHit {
                uid: extract_text(&doc, self.fields.uid),
                kind: extract_text(&doc, self.fields.kind),
                title: extract_text(&doc, self.fields.title),
                vault_uid: extract_text(&doc, self.fields.vault_uid),
                score,
            });
        }
        Ok(hits)
    }

    /// Total doc count — useful for status / health checks.
    pub fn doc_count(&self) -> usize {
        self.reader.searcher().num_docs() as usize
    }

    /// Returns `true` if this instance has a writer (i.e. was opened via
    /// `open_or_create`). Reader-only instances return `false`.
    pub fn has_writer(&self) -> bool {
        self.writer.is_some()
    }

    fn write_full_corpus(
        &self,
        writer: &mut tantivy::IndexWriter,
        store: &GraphStore,
    ) -> Result<usize, TantivyError> {
        let mut count = 0usize;

        let notes = store
            .list_notes(None)
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;
        for note in &notes {
            let sections = store.sections_in_note(&note.uid).unwrap_or_default();

            // Try to read the note body from disk for full-text indexing.
            let body_from_disk: Option<String> =
                store.lookup_vault(&note.vault_uid).ok().and_then(|vault| {
                    let path = std::path::Path::new(&vault.root_path).join(&note.file_path);
                    std::fs::read_to_string(path).ok()
                });

            let note_body = body_from_disk.as_deref().unwrap_or(&note.title);
            writer.add_document(doc!(
                self.fields.uid => note.uid.clone(),
                self.fields.kind => "note".to_string(),
                self.fields.title => note.title.clone(),
                self.fields.body => note_body.to_string(),
                self.fields.vault_uid => note.vault_uid.clone(),
                self.fields.note_uid => note.uid.clone(),
            ))?;
            count += 1;

            let headings = store.headings_in_note(&note.uid).unwrap_or_default();
            for h in &headings {
                writer.add_document(doc!(
                    self.fields.uid => h.uid.clone(),
                    self.fields.kind => "heading".to_string(),
                    self.fields.title => h.text.clone(),
                    self.fields.body => h.text.clone(),
                    self.fields.vault_uid => note.vault_uid.clone(),
                    self.fields.note_uid => note.uid.clone(),
                ))?;
                count += 1;
            }

            // Section body sourcing priority:
            // 1. `Section.text_content` from the graph — canonical view of
            //    what the indexer saw, survives even if the file is later
            //    moved or deleted on disk.
            // 2. Slice from `body_from_disk` if (1) is empty (handles
            //    sections inserted by older code versions that pre-date
            //    the text_content field).
            let body_lines: Vec<&str> = body_from_disk
                .as_deref()
                .map(|b| b.lines().collect())
                .unwrap_or_default();
            for s in &sections {
                let section_text = if !s.text_content.is_empty() {
                    s.text_content.clone()
                } else {
                    slice_section_lines(&body_lines, s.start_line, s.end_line)
                };
                writer.add_document(doc!(
                    self.fields.uid => s.uid.clone(),
                    self.fields.kind => "section".to_string(),
                    self.fields.title => String::new(),
                    self.fields.body => section_text,
                    self.fields.vault_uid => note.vault_uid.clone(),
                    self.fields.note_uid => note.uid.clone(),
                ))?;
                count += 1;
            }
        }

        // Tags — one doc each. The kind discriminator lets clients
        // filter `tag` hits if needed.
        let tags = store
            .list_tags(None)
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;
        for tag in &tags {
            writer.add_document(doc!(
                self.fields.uid => tag.uid.clone(),
                self.fields.kind => "tag".to_string(),
                self.fields.title => tag.name.clone(),
                self.fields.body => tag.name.clone(),
                self.fields.vault_uid => tag.vault_uid.clone(),
                self.fields.note_uid => String::new(),
            ))?;
            count += 1;
        }

        Ok(count)
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("uid", STRING | STORED);
    builder.add_text_field("kind", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("body", TEXT);
    builder.add_text_field("vault_uid", STRING | STORED);
    builder.add_text_field("note_uid", STRING);
    builder.build()
}

fn lookup_fields(schema: &Schema) -> Fields {
    Fields {
        uid: schema.get_field("uid").expect("uid field"),
        kind: schema.get_field("kind").expect("kind field"),
        title: schema.get_field("title").expect("title field"),
        body: schema.get_field("body").expect("body field"),
        vault_uid: schema.get_field("vault_uid").expect("vault_uid field"),
        note_uid: schema.get_field("note_uid").expect("note_uid field"),
    }
}

fn extract_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default()
}

fn slice_section_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || start as usize > lines.len() {
        return String::new();
    }
    let end = (end as usize).min(lines.len());
    let start = (start - 1) as usize;
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

/// Escape characters the Tantivy query parser treats specially. Used as
/// the fallback when the user's raw query fails to parse.
fn escape_query(q: &str) -> String {
    // Tantivy's query parser doesn't support backslash-escaping for all
    // special chars (notably `:` as the field separator). Split the input
    // into whitespace-delimited tokens and quote any token that contains
    // special characters. This preserves adjacency within each token
    // while avoiding parser errors.
    let specials = [
        '+', '-', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\', '/',
    ];
    let mut out = String::with_capacity(q.len() + 16);
    for (i, token) in q.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if token.chars().any(|c| specials.contains(&c)) {
            let clean: String = token.chars().filter(|c| *c != '"').collect();
            out.push('"');
            out.push_str(&clean);
            out.push('"');
        } else {
            out.push_str(token);
        }
    }
    out
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Note, NoteKind, Tag, Vault};
    use tempfile::tempdir;

    fn make_store_with_notes() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:t".to_string(),
                name: "t".to_string(),
                root_path: "/t".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        for (uid, title) in [
            ("note:1", "Auth Service Design"),
            ("note:2", "Database Migration Strategy"),
            ("note:3", "Pairing Protocol Notes"),
        ] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: "vlt:t".to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::Design,
                    word_count: 10,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                })
                .unwrap();
        }
        store
            .insert_tag(&Tag {
                uid: "tag:1".to_string(),
                vault_uid: "vlt:t".to_string(),
                name: "auth".to_string(),
            })
            .unwrap();
        store
    }

    #[test]
    fn open_or_create_creates_empty_index() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(idx.doc_count(), 0);
    }

    #[test]
    fn reindex_then_search_finds_notes_by_title() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let store = make_store_with_notes();
        let n = idx.reindex_from_store(&store).unwrap();
        assert!(n >= 3, "expected at least 3 docs, got {n}");

        let hits = idx.search("auth", 10).unwrap();
        let titles: Vec<&str> = hits.iter().map(|h| h.title.as_str()).collect();
        assert!(
            titles.contains(&"Auth Service Design") || hits.iter().any(|h| h.kind == "tag"),
            "expected an auth hit; got {titles:?}"
        );
    }

    #[test]
    fn search_returns_empty_for_empty_query() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let hits = idx.search("", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn lenient_parser_handles_special_chars() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let store = make_store_with_notes();
        idx.reindex_from_store(&store).unwrap();
        // "auth:v2" would break the strict parser (treats `auth:` as a
        // field qualifier with no matching field). Lenient escape kicks in.
        let _ = idx.search("auth:v2", 5).unwrap();
    }

    #[test]
    fn update_note_replaces_old_docs() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:x",
            "Original Title",
            "vlt:t",
            &["original body content".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let hits = idx.search("original", 5).unwrap();
        assert!(!hits.is_empty(), "should find original");

        idx.update_note(
            "note:x",
            "Renamed",
            "vlt:t",
            &["completely fresh body".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let old_hits = idx.search("original", 5).unwrap();
        assert!(old_hits.is_empty(), "old doc should be replaced");
        let new_hits = idx.search("fresh", 5).unwrap();
        assert!(!new_hits.is_empty(), "new doc should be searchable");
    }

    #[test]
    fn title_match_ranks_above_body_match() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:a",
            "Auth Service",
            "vlt:t",
            &["some unrelated body content".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        idx.update_note(
            "note:b",
            "Unrelated Title",
            "vlt:t",
            &["auth is mentioned in the body".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let hits = idx.search("auth", 10).unwrap();
        assert!(
            hits.len() >= 2,
            "expected at least 2 hits, got {}",
            hits.len()
        );
        assert_eq!(hits[0].uid, "note:a", "title match should rank first");
    }

    #[test]
    fn escape_query_preserves_terms() {
        assert_eq!(escape_query("auth/v2"), "\"auth/v2\"");
        assert_eq!(escape_query("foo:bar"), "\"foo:bar\"");
        assert_eq!(escape_query("hello world"), "hello world");
        assert_eq!(
            escape_query("hello foo:bar world"),
            "hello \"foo:bar\" world"
        );
    }

    #[test]
    fn remove_note_clears_all_docs_for_that_note() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:y",
            "Doomed",
            "vlt:t",
            &["body of doomed note".to_string()],
            &[("head:y:1".to_string(), "Doomed Heading".to_string())],
            &[],
            &[],
        )
        .unwrap();
        assert!(!idx.search("doomed", 5).unwrap().is_empty());
        idx.remove_note("note:y").unwrap();
        assert!(idx.search("doomed", 5).unwrap().is_empty());
    }

    #[test]
    fn reader_only_can_search_while_writer_is_held() {
        let dir = tempdir().unwrap();
        // Open a full (writer) instance, write some data, and keep it alive.
        let writer_idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        writer_idx
            .update_note(
                "note:rw",
                "Concurrent Read Test",
                "vlt:t",
                &["body for concurrent read test".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert!(writer_idx.has_writer());

        // Open a second, reader-only instance on the same directory.
        let reader_idx = TantivyIndex::open_reader_only(dir.path()).unwrap();
        assert!(!reader_idx.has_writer());

        // The reader should be able to search the data committed by the writer.
        let hits = reader_idx.search("concurrent", 5).unwrap();
        assert!(
            !hits.is_empty(),
            "reader-only instance should find committed data"
        );
        assert_eq!(hits[0].title, "Concurrent Read Test");

        // Doc count should also work.
        assert!(reader_idx.doc_count() > 0);
    }

    #[test]
    fn reader_only_rejects_write_operations() {
        let dir = tempdir().unwrap();
        // Create the index first so reader_only can open it.
        let _writer = TantivyIndex::open_or_create(dir.path()).unwrap();
        drop(_writer);

        let reader = TantivyIndex::open_reader_only(dir.path()).unwrap();

        // update_note should fail with WriterUnavailable.
        let err = reader
            .update_note("note:x", "X", "vlt:t", &[], &[], &[], &[])
            .unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );

        // remove_note should fail with WriterUnavailable.
        let err = reader.remove_note("note:x").unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );

        // reindex_from_store should fail with WriterUnavailable.
        let store = GraphStore::in_memory().unwrap();
        let err = reader.reindex_from_store(&store).unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );
    }

    #[test]
    fn open_reader_only_fails_on_nonexistent_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = TantivyIndex::open_reader_only(&missing);
        assert!(result.is_err(), "should fail on missing directory");
    }
}

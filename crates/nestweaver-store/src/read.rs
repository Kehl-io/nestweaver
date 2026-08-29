use std::collections::{HashMap, HashSet};

use lbug::Value;
use nestweaver_schema::{
    Contract, EntryPointKind, Heading, Note, NoteKind, Project, Repo, Section, Service, Symbol,
    SymbolKind, Tag, Vault, Visibility,
};
use serde::Serialize;

use crate::db::GraphStore;
use crate::error::StoreError;

/// A lightweight symbol representation used for clustering.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolBasic {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
    /// Needed because `file_path` is REPO-RELATIVE. Any per-file lookup keyed
    /// on the path alone matches the wrong repo whenever two repos share a
    /// name — `src/main.rs`, `README.md` (nw-233). Parsing it back out of `uid`
    /// is not an option: `repo_uid` itself contains colons.
    pub repo_uid: String,
}

/// Edge data for clustering: (source_uid, target_uid, confidence).
pub type CodeEdge = (String, String, f64);

/// Edge data with type and evidence: (source_uid, target_uid, edge_type, confidence, evidence_json).
pub type TypedEdge = (String, String, String, f64, String);

/// Combined symbols and edges for clustering algorithms.
pub type CodeGraph = (Vec<SymbolBasic>, Vec<CodeEdge>);

/// Caller and callee names keyed by symbol UID for batched summary rendering.
pub type SummaryAdjacency = HashMap<String, (Vec<String>, Vec<String>)>;

/// A single inbound wikilink to a target Note — the source side of the edge.
#[derive(Debug, Clone, Serialize)]
pub struct BacklinkRow {
    pub source_note_uid: String,
    pub source_note_title: String,
    pub source_note_path: String,
    pub source_section_uid: String,
    pub confidence: f32,
    pub display: Option<String>,
}

/// A wikilink edge whose resolution is suspect (confidence below 1.0).
/// Surfaced by `brain_broken_links` so the source note + link text are known
/// without a follow-up read.
#[derive(Debug, Clone, Serialize)]
pub struct BrokenWikilinkRow {
    pub source_uid: String,
    pub source_path: String,
    pub source_title: String,
    pub wikilink_text: String,
    pub confidence: f32,
    /// The note UID this edge currently points at (the low-confidence target).
    pub current_target_uid: String,
}

/// A lightweight note row used by orphan detection and topic clustering.
/// Only the fields those queries need — no body load.
#[derive(Debug, Clone, Serialize)]
pub struct NoteLite {
    pub uid: String,
    pub title: String,
    pub file_path: String,
    pub vault_uid: String,
    pub pagerank_score: f64,
}

/// A cross-repo reference between two symbols.
#[derive(Debug, Clone, Serialize)]
pub struct CrossRepoRef {
    pub source_uid: String,
    pub source_name: String,
    /// Populated only when the query returns repo context (e.g. `list_all_cross_repo_links`).
    pub source_repo_uid: Option<String>,
    pub target_uid: String,
    pub target_name: String,
    /// Populated only when the query returns repo context (e.g. `list_all_cross_repo_links`).
    pub target_repo_uid: Option<String>,
    pub link_type: String,
    pub confidence: f32,
}

/// Extract a String value from a row column, returning an error on type mismatch or out-of-bounds.
/// Corruption canary for a returned string. An embedded NUL byte is never
/// valid in any column NestWeaver stores (symbol names, uids, paths, note
/// bodies, signatures — none contain NUL), and both corruption patterns
/// observed from the storage engine's partial string scans (LadybugDB #678)
/// contained NUL: fully-zeroed values and garbage-prefixed ones alike. It is a
/// one-byte scan with zero false-positive risk. Broader control-character
/// checks are deliberately left to field-aware callers (a note body may hold a
/// form feed); NUL is the universal, safe-to-fail-on signal (Google SRE Ch. 26
/// on validating only invariants that cause user-facing devastation).
pub(crate) fn string_is_corrupt(s: &str) -> bool {
    s.as_bytes().contains(&0)
}

pub(crate) fn extract_string(row: &[Value], idx: usize) -> Result<String, StoreError> {
    let val = row
        .get(idx)
        .ok_or_else(|| StoreError::Query(format!("column {idx} out of bounds")))?;
    match val {
        Value::String(s) if string_is_corrupt(s) => Err(StoreError::CorruptValue {
            column: idx,
            reason: "embedded NUL byte (storage-engine partial-scan corruption)".to_string(),
        }),
        Value::String(s) => Ok(s.clone()),
        Value::Null(_) => Ok(String::new()),
        other => Err(StoreError::Query(format!(
            "expected String at column {idx}, got {other:?}"
        ))),
    }
}

fn extract_opt_string(row: &[Value], idx: usize) -> Result<Option<String>, StoreError> {
    let val = row
        .get(idx)
        .ok_or_else(|| StoreError::Query(format!("column {idx} out of bounds")))?;
    match val {
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) if string_is_corrupt(s) => Err(StoreError::CorruptValue {
            column: idx,
            reason: "embedded NUL byte (storage-engine partial-scan corruption)".to_string(),
        }),
        Value::String(s) => Ok(Some(s.clone())),
        Value::Null(_) => Ok(None),
        other => Err(StoreError::Query(format!(
            "expected String at column {idx}, got {other:?}"
        ))),
    }
}

fn extract_i64(row: &[Value], idx: usize) -> Result<i64, StoreError> {
    let val = row
        .get(idx)
        .ok_or_else(|| StoreError::Query(format!("column {idx} out of bounds")))?;
    match val {
        Value::Int64(n) => Ok(*n),
        Value::Null(_) => Ok(0),
        other => Err(StoreError::Query(format!(
            "expected Int64 at column {idx}, got {other:?}"
        ))),
    }
}

fn extract_f64(row: &[Value], idx: usize) -> Result<f64, StoreError> {
    let val = row
        .get(idx)
        .ok_or_else(|| StoreError::Query(format!("column {idx} out of bounds")))?;
    match val {
        Value::Double(n) => Ok(*n),
        Value::Float(n) => Ok(*n as f64),
        Value::Int64(n) => Ok(*n as f64),
        Value::Null(_) => Ok(0.0),
        other => Err(StoreError::Query(format!(
            "expected numeric at column {idx}, got {other:?}"
        ))),
    }
}

fn parse_symbol_kind(s: &str) -> SymbolKind {
    match s {
        "Function" => SymbolKind::Function,
        "Class" => SymbolKind::Class,
        "Method" => SymbolKind::Method,
        "Interface" => SymbolKind::Interface,
        "Trait" => SymbolKind::Trait,
        "Enum" => SymbolKind::Enum,
        "Module" => SymbolKind::Module,
        "Extension" => SymbolKind::Extension,
        "Constant" => SymbolKind::Constant,
        "Property" => SymbolKind::Property,
        "TypeAlias" => SymbolKind::TypeAlias,
        "Variable" => SymbolKind::Variable,
        other => {
            tracing::warn!("unknown SymbolKind '{}', falling back to Function", other);
            SymbolKind::Function
        }
    }
}

/// Parse the `visibility` column back into a [`Visibility`].
///
/// nw-291: an empty value is what a row written before the column existed reads
/// back as, and what a symbol the parser could not classify writes. Both mean
/// "not stated", which is exactly `Inferred` — so the pre-migration behaviour
/// is preserved without pretending an unknown value is `Public`.
fn parse_visibility(s: &str) -> Visibility {
    match s {
        "public" => Visibility::Public,
        "internal" => Visibility::Internal,
        "protected" => Visibility::Protected,
        "private" => Visibility::Private,
        _ => Visibility::Inferred,
    }
}

fn parse_entry_point_kind(s: &str) -> Option<EntryPointKind> {
    match s {
        "main" => Some(EntryPointKind::Main),
        "http_handler" => Some(EntryPointKind::HttpHandler),
        "event_listener" => Some(EntryPointKind::EventListener),
        "test_entry" => Some(EntryPointKind::TestEntry),
        "lambda_handler" => Some(EntryPointKind::LambdaHandler),
        "cron_job" => Some(EntryPointKind::CronJob),
        "cli_command" => Some(EntryPointKind::CliCommand),
        _ => None,
    }
}

/// Build a Symbol from a query row with columns:
/// uid, name, kind, repo_uid, file_path, start_line, end_line, signature, summary, content_hash, pagerank_score, is_entry_point, entry_point_kind
pub(crate) fn row_to_symbol(row: &[Value]) -> Result<Symbol, StoreError> {
    let uid = extract_string(row, 0)?;
    let name = extract_string(row, 1)?;
    let kind_str = extract_string(row, 2)?;
    let kind = parse_symbol_kind(&kind_str);
    let repo_uid = extract_string(row, 3)?;
    let file_path = extract_string(row, 4)?;
    let start_line = u32::try_from(extract_i64(row, 5)?).unwrap_or(0);
    let end_line = u32::try_from(extract_i64(row, 6)?).unwrap_or(0);
    let signature = extract_string(row, 7)?;
    let summary = extract_opt_string(row, 8)?;
    let content_hash = extract_string(row, 9)?;
    let pagerank_score = extract_f64(row, 10)?;
    let iep_str = extract_string(row, 11).unwrap_or_default();
    let is_entry_point = iep_str == "true";
    let epk_str = extract_opt_string(row, 12).unwrap_or(None);
    let entry_point_kind = epk_str.as_deref().and_then(parse_entry_point_kind);
    // Column 13 (framework_hint) is optional: older queries that don't SELECT
    // it leave the row short, in which case `get` returns None.
    let framework_hint = row
        .get(13)
        .and_then(|_| extract_opt_string(row, 13).ok().flatten())
        .and_then(|s| parse_framework_hint(&s));
    // Column 14 (canonical_id) is optional: added in Phase 4 migration.
    let canonical_id = row
        .get(14)
        .and_then(|_| extract_opt_string(row, 14).ok().flatten())
        .filter(|s| !s.is_empty());
    // Column 15 (visibility) is optional: added by the nw-291 migration.
    let visibility = row
        .get(15)
        .and_then(|_| extract_opt_string(row, 15).ok().flatten())
        .map(|s| parse_visibility(&s))
        .unwrap_or(Visibility::Inferred);

    Ok(Symbol {
        uid,
        name,
        kind,
        repo_uid,
        file_path,
        start_line,
        end_line,
        signature,
        summary,
        content_hash,
        embedding: None,
        pagerank_score: Some(pagerank_score),
        is_entry_point,
        entry_point_kind,
        visibility,
        type_info: None,
        framework_hint,
        canonical_id,
    })
}

/// Parse a `"framework:role"` string (as stored in the `framework_hint`
/// column) back into a [`FrameworkHint`]. Empty / malformed values yield None.
fn parse_framework_hint(s: &str) -> Option<nestweaver_schema::FrameworkHint> {
    let (framework, role) = s.split_once(':')?;
    if framework.is_empty() {
        return None;
    }
    Some(nestweaver_schema::FrameworkHint {
        framework: framework.to_string(),
        role: role.to_string(),
    })
}

pub(crate) const SYMBOL_COLUMNS: &str = "s.uid, s.name, s.kind, s.repo_uid, s.file_path, s.start_line, s.end_line, \
     s.signature, s.summary, s.content_hash, s.pagerank_score, s.is_entry_point, s.entry_point_kind, \
     s.framework_hint, s.canonical_id, s.visibility";

pub(crate) const NOTE_COLUMNS: &str = "n.uid, n.vault_uid, n.file_path, n.title, n.note_kind, \
     n.word_count, n.content_hash, n.frontmatter, n.created_at, n.modified_at, n.pagerank_score";

pub(crate) const HEADING_COLUMNS: &str = "h.uid, h.note_uid, h.level, h.text, h.slug, \
     h.start_line, h.end_line, h.content_hash";

pub(crate) const SECTION_COLUMNS: &str = "s.uid, s.note_uid, s.heading_uid, s.start_line, \
     s.end_line, s.text_hash, s.text_content, s.word_count, s.pagerank_score";

pub(crate) fn row_to_heading(row: &[Value]) -> Result<Heading, StoreError> {
    let uid = extract_string(row, 0)?;
    let note_uid = extract_string(row, 1)?;
    let level = u8::try_from(extract_i64(row, 2)?).unwrap_or(1);
    let text = extract_string(row, 3)?;
    let slug = extract_string(row, 4)?;
    let start_line = u32::try_from(extract_i64(row, 5)?).unwrap_or(0);
    let end_line = u32::try_from(extract_i64(row, 6)?).unwrap_or(0);
    let content_hash = extract_string(row, 7)?;
    Ok(Heading {
        uid,
        note_uid,
        level,
        text,
        slug,
        start_line,
        end_line,
        content_hash,
        embedding: None,
    })
}

pub(crate) fn row_to_section(row: &[Value]) -> Result<Section, StoreError> {
    let uid = extract_string(row, 0)?;
    let note_uid = extract_string(row, 1)?;
    let heading_uid = extract_opt_string(row, 2)?;
    let start_line = u32::try_from(extract_i64(row, 3)?).unwrap_or(0);
    let end_line = u32::try_from(extract_i64(row, 4)?).unwrap_or(0);
    let text_hash = extract_string(row, 5)?;
    let text_content = extract_string(row, 6)?;
    let word_count = u32::try_from(extract_i64(row, 7)?).unwrap_or(0);
    let pagerank_score = extract_f64(row, 8)?;
    Ok(Section {
        uid,
        note_uid,
        heading_uid,
        start_line,
        end_line,
        text_hash,
        text_content,
        word_count,
        pagerank_score: Some(pagerank_score),
    })
}

fn parse_note_kind(s: &str) -> NoteKind {
    match s {
        "General" => NoteKind::General,
        "PRD" => NoteKind::Prd,
        "Design" => NoteKind::Design,
        "Meeting" => NoteKind::Meeting,
        "Journal" => NoteKind::Journal,
        "AgentConfig" => NoteKind::AgentConfig,
        other => {
            tracing::warn!("unknown NoteKind '{}', falling back to General", other);
            NoteKind::General
        }
    }
}

/// Build a Note from a query row with columns:
/// uid, vault_uid, file_path, title, note_kind, word_count, content_hash,
/// frontmatter, created_at, modified_at, pagerank_score
pub(crate) fn row_to_note(row: &[Value]) -> Result<Note, StoreError> {
    let uid = extract_string(row, 0)?;
    let vault_uid = extract_string(row, 1)?;
    let file_path = extract_string(row, 2)?;
    let title = extract_string(row, 3)?;
    let kind_str = extract_string(row, 4)?;
    let note_kind = parse_note_kind(&kind_str);
    let word_count = u32::try_from(extract_i64(row, 5)?).unwrap_or(0);
    let content_hash = extract_string(row, 6)?;
    let frontmatter = extract_opt_string(row, 7)?;
    let created_at = extract_opt_string(row, 8)?;
    let modified_at = extract_opt_string(row, 9)?;
    let pagerank_score = extract_f64(row, 10)?;

    Ok(Note {
        uid,
        vault_uid,
        file_path,
        title,
        note_kind,
        word_count,
        content_hash,
        frontmatter,
        created_at,
        modified_at,
        pagerank_score: Some(pagerank_score),
        embedding: None,
    })
}

impl GraphStore {
    pub fn lookup_symbol(&self, uid: &str) -> Result<Symbol, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Symbol {{uid: $uid}}) RETURN {}", SYMBOL_COLUMNS);
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => row_to_symbol(&row),
            None => Err(StoreError::NotFound),
        }
    }

    /// Fetch multiple symbols in a single query, keyed by UID.
    ///
    /// Drives primary-key probes through one `UNWIND` query instead of N
    /// individual lookups.
    /// UIDs not found in the graph are simply absent from the returned map.
    /// Returns an empty map when `uids` is empty.
    pub fn batch_lookup_symbols(
        &self,
        uids: &[&str],
    ) -> Result<std::collections::HashMap<String, Symbol>, StoreError> {
        self.batch_lookup_symbols_impl(uids, false)
    }

    /// Exact variant for trust-sensitive snapshots.
    ///
    /// In addition to using primary-key probes, this rejects duplicate
    /// requests, missing rows, unexpected rows, and duplicate result rows.
    pub(crate) fn batch_lookup_symbols_exact(
        &self,
        uids: &[&str],
    ) -> Result<std::collections::HashMap<String, Symbol>, StoreError> {
        self.batch_lookup_symbols_impl(uids, true)
    }

    fn batch_lookup_symbols_impl(
        &self,
        uids: &[&str],
        require_exact: bool,
    ) -> Result<std::collections::HashMap<String, Symbol>, StoreError> {
        if uids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let expected = if require_exact {
            let mut expected = std::collections::HashSet::with_capacity(uids.len());
            for uid in uids {
                if !expected.insert(*uid) {
                    return Err(StoreError::Query(format!(
                        "batch_lookup_symbols_exact: duplicate requested UID {uid}"
                    )));
                }
            }
            Some(expected)
        } else {
            None
        };
        let conn = self.conn()?;
        // Drive PRIMARY-KEY point lookups via UNWIND rather than a
        // `WHERE s.uid IN [...]` scan-with-filter. Two reasons:
        //   1. Correctness: the storage engine's partial sequential string
        //      scans can return garbled non-PK string values after
        //      delete+checkpoint cycles (re-indexing), while primary-key point
        //      lookups return correct values for the same rows. Driving the
        //      lookup through the PK index is what keeps names/paths intact.
        //   2. Speed: N index probes inside one query plan instead of a
        //      filtered scan or a separately planned query per UID.
        let in_list: String = uids
            .iter()
            .map(|u| format!("'{}'", u.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let q = format!(
            "UNWIND [{}] AS want_uid \
             MATCH (s:Symbol {{uid: want_uid}}) RETURN {}",
            in_list, SYMBOL_COLUMNS
        );
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(format!("batch_lookup_symbols: {e}")))?;
        let mut map = std::collections::HashMap::with_capacity(uids.len());
        for row in result {
            let sym = row_to_symbol(&row)?;
            if let Some(expected) = &expected
                && !expected.contains(sym.uid.as_str())
            {
                return Err(StoreError::Query(format!(
                    "batch_lookup_symbols_exact: unexpected symbol UID {}",
                    sym.uid
                )));
            }
            let uid = sym.uid.clone();
            if map.insert(uid.clone(), sym).is_some() && require_exact {
                return Err(StoreError::Query(format!(
                    "batch_lookup_symbols_exact: duplicate result row for {uid}"
                )));
            }
        }
        if expected.is_some() {
            for uid in uids {
                if !map.contains_key(*uid) {
                    return Err(StoreError::Query(format!(
                        "batch_lookup_symbols_exact: missing symbol UID {uid}"
                    )));
                }
            }
        }
        Ok(map)
    }

    pub fn lookup_symbols_by_name(&self, name: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.name = $name RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("name", Value::String(name.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    pub fn list_repos(&self, instance_id: Option<&str>) -> Result<Vec<Repo>, StoreError> {
        let conn = self.conn()?;
        let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, r.instance_id, \
                    r.name, r.root_path";
        let result = if let Some(iid) = instance_id {
            let q = format!("MATCH (r:Repo) WHERE r.instance_id = $iid RETURN {cols}");
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            conn.execute(&mut stmt, vec![("iid", Value::String(iid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?
        } else {
            let q = format!("MATCH (r:Repo) RETURN {cols}");
            conn.query(&q)
                .map_err(|e| StoreError::Query(e.to_string()))?
        };
        let rows: Vec<Repo> = result
            .map(|row| {
                let uid = extract_string(&row, 0)?;
                let url = extract_string(&row, 1)?;
                let indexed_sha = extract_string(&row, 2)?;
                let staleness = u32::try_from(extract_i64(&row, 3)?).unwrap_or(0);
                let instance_id = extract_string(&row, 4)?;
                let name = extract_opt_string(&row, 5).unwrap_or(None);
                // Rows predating the migration hold '' — mapped to None.
                let root_path = extract_opt_string(&row, 6).unwrap_or(None);
                Ok(Repo {
                    uid,
                    url,
                    indexed_sha,
                    staleness_commits_behind: staleness,
                    instance_id,
                    name,
                    root_path,
                })
            })
            .collect::<Result<_, StoreError>>()?;

        // nw-043 instrumentation: a Repo row whose provenance doesn't match this
        // handle's DB is the isolation-anomaly signature. Debug builds trace every
        // listing with handle provenance so a recurrence in ANY suite (not just the
        // e2e guard) is attributable to a specific handle + path. Zero cost unless
        // RUST_LOG=nw043=trace.
        #[cfg(debug_assertions)]
        if let Some(db_path) = self.db_path() {
            for r in &rows {
                tracing::trace!(target: "nw043",
                    db = %db_path.display(), uid = %r.uid, url = %r.url,
                    "list_repos row provenance");
            }
        }

        Ok(rows)
    }

    pub fn list_services(&self, instance_id: Option<&str>) -> Result<Vec<Service>, StoreError> {
        let conn = self.conn()?;
        let svc_cols = "svc.uid, svc.name, svc.repo_uid, svc.summary, svc.summary_hash";
        let result = if let Some(iid) = instance_id {
            let q = format!(
                "MATCH (svc:Service) WHERE svc.repo_uid IN \
                 (MATCH (r:Repo {{instance_id: $iid}}) RETURN r.uid) \
                 RETURN DISTINCT {svc_cols}"
            );
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            conn.execute(&mut stmt, vec![("iid", Value::String(iid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?
        } else {
            let q = format!("MATCH (svc:Service) RETURN {svc_cols}");
            conn.query(&q)
                .map_err(|e| StoreError::Query(e.to_string()))?
        };
        result
            .map(|row| {
                let uid = extract_string(&row, 0)?;
                let name = extract_string(&row, 1)?;
                let repo_uid = extract_string(&row, 2)?;
                let summary = extract_opt_string(&row, 3)?;
                let summary_hash = extract_opt_string(&row, 4)?;
                Ok(Service {
                    uid,
                    name,
                    repo_uid,
                    summary,
                    summary_hash,
                    embedding: None,
                })
            })
            .collect()
    }

    /// Returns all Symbol nodes that have a CALLS edge pointing TO `uid`.
    pub fn callers_of(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol)-[:CALLS]->(t:Symbol {{uid: $uid}}) RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Fetch caller and callee names for many symbols with a constant number
    /// of query plans.
    ///
    /// Summary generation only needs names, not fully hydrated symbols. This
    /// avoids issuing one inbound query plus three outbound queries per symbol
    /// while retaining the same CALLS/IMPORTS/CROSS_REPO_LINK semantics as
    /// [`callers_of`](Self::callers_of) and [`callees_of`](Self::callees_of).
    pub fn summary_adjacency_by_uid(
        &self,
        uids: &[String],
    ) -> Result<SummaryAdjacency, StoreError> {
        let mut adjacency = HashMap::with_capacity(uids.len());
        for uid in uids {
            adjacency
                .entry(uid.clone())
                .or_insert_with(|| (Vec::new(), Vec::new()));
        }
        if uids.is_empty() {
            return Ok(adjacency);
        }

        let in_list = uids
            .iter()
            .map(|uid| format!("'{}'", uid.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let conn = self.conn()?;

        let callers_query = format!(
            "UNWIND [{in_list}] AS want_uid \
             MATCH (caller:Symbol)-[:CALLS]->(target:Symbol {{uid: want_uid}}) \
             RETURN target.uid, caller.name"
        );
        let callers = conn
            .query(&callers_query)
            .map_err(|error| StoreError::Query(format!("summary callers batch: {error}")))?;
        for row in callers {
            let uid = extract_string(&row, 0)?;
            let name = extract_string(&row, 1)?;
            if let Some((names, _)) = adjacency.get_mut(&uid) {
                names.push(name);
            }
        }

        for edge_type in ["CALLS", "IMPORTS", "CROSS_REPO_LINK"] {
            let callees_query = format!(
                "UNWIND [{in_list}] AS want_uid \
                 MATCH (source:Symbol {{uid: want_uid}})-[:{edge_type}]->(target:Symbol) \
                 RETURN source.uid, target.name"
            );
            let callees = conn.query(&callees_query).map_err(|error| {
                StoreError::Query(format!("summary {edge_type} batch: {error}"))
            })?;
            for row in callees {
                let uid = extract_string(&row, 0)?;
                let name = extract_string(&row, 1)?;
                if let Some((_, names)) = adjacency.get_mut(&uid)
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
        }

        for (callers, callees) in adjacency.values_mut() {
            callers.sort();
            callers.dedup();
            callees.sort();
        }
        Ok(adjacency)
    }

    /// Returns the set of service UIDs that have at least one caller whose
    /// `file_path` contains "test" or "spec" (case-insensitive). Used by the
    /// gaps endpoint to compute the untested-services set in a single query
    /// instead of N individual `callers_of` calls.
    pub fn tested_service_uids(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (caller:Symbol)-[:CALLS]->(callee:Symbol)\
                 <-[:SERVICE_HAS_SYMBOL]-(svc:Service) \
                 RETURN svc.uid, caller.file_path";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut tested = HashSet::new();
        for row in result {
            let svc_uid = extract_string(&row, 0)?;
            let file_path = extract_string(&row, 1)?;
            let lc = file_path.to_lowercase();
            if lc.contains("test") || lc.contains("spec") {
                tested.insert(svc_uid);
            }
        }
        Ok(tested)
    }

    /// The symbols a service owns, via `SERVICE_HAS_SYMBOL`.
    ///
    /// nw-311: `service-summary --help` says "Show a service summary with entry
    /// points" and no code path anywhere ever looked for one — `Service` has six
    /// fields and none of them is an entry point. The data was one query away
    /// the whole time: the edge is written at `write.rs` and `Symbol` already
    /// carries `is_entry_point`/`entry_point_kind`. Filtering happens in the
    /// caller so this stays the general traversal.
    pub fn symbols_in_service(&self, service_uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (svc:Service {{uid: $uid}})-[:SERVICE_HAS_SYMBOL]->(s:Symbol) RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("uid", Value::String(service_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Look up a symbol by its canonical_id (Phase 4 cross-boundary matching).
    /// Returns `None` if no symbol has this canonical_id.
    pub fn symbol_by_canonical_id(&self, canonical_id: &str) -> Result<Option<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.canonical_id = $cid RETURN {} LIMIT 1",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![("cid", Value::String(canonical_id.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => Ok(Some(row_to_symbol(&row)?)),
            None => Ok(None),
        }
    }

    /// Return ALL symbols matching a canonical_id (not just the first).
    /// Used by cross-repo impact analysis where the same canonical_id
    /// appears in multiple repos (e.g., a shared interface).
    pub fn find_symbols_by_canonical_id(
        &self,
        canonical_id: &str,
    ) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.canonical_id = $cid RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("cid", Value::String(canonical_id.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Find a symbol by name and file path. Used as a fallback when
    /// canonical_id lookup fails (e.g., repo URL mismatch between the
    /// diff analysis and the indexed graph).
    pub fn find_symbol_by_name_and_file(
        &self,
        name: &str,
        file_path: &str,
    ) -> Result<Option<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.name = $name AND s.file_path = $fp RETURN {} LIMIT 1",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![
                    ("name", Value::String(name.to_string())),
                    ("fp", Value::String(file_path.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => Ok(Some(row_to_symbol(&row)?)),
            None => Ok(None),
        }
    }

    /// Returns all Symbol nodes that have any incoming edge (CALLS, IMPORTS,
    /// EXTENDS_SYM, IMPLEMENTS_SYM, INCLUDES_SYM) pointing TO `uid`.
    /// Used by impact analysis for SymbolRemoved/ExportRemoved changes.
    pub fn references_to(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let edge_types = [
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "INCLUDES_SYM",
            // Cross-repo consumers must surface in impact analysis so pre-push /
            // ImpactAnalysis reports breaking changes across repo boundaries.
            "CROSS_REPO_LINK",
        ];
        let mut all: Vec<Symbol> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for et in &edge_types {
            let q = format!(
                "MATCH (s:Symbol)-[:{}]->(t:Symbol {{uid: $uid}}) RETURN {}",
                et, SYMBOL_COLUMNS
            );
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            let result = conn
                .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            for row in result {
                let sym = row_to_symbol(&row)?;
                if seen.insert(sym.uid.clone()) {
                    all.push(sym);
                }
            }
        }
        Ok(all)
    }

    /// Returns the set of file paths (within `repo_uid`) that contain at least
    /// one symbol with a cross-file resolved edge pointing INTO a symbol in
    /// `file_path`. In other words, the 1-hop reverse-dependents of `file_path`.
    ///
    /// Only cross-file edges are considered: edges whose source and target live
    /// in the same file are excluded (`s.file_path <> $fp`), so the result never
    /// contains `file_path` itself. `MEMBER_OF` (structural, intra-file) and
    /// `CROSS_REPO_LINK` (cross-repo, handled separately) are not traversed.
    ///
    /// Used by incremental re-resolution to find files whose resolved edges may
    /// need to be rebuilt after `file_path` changes.
    pub fn files_referencing_file(
        &self,
        repo_uid: &str,
        file_path: &str,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let edge_types = [
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "INCLUDES_SYM",
            "USES",
            "ACCESSES",
        ];
        let mut files: std::collections::HashSet<String> = std::collections::HashSet::new();
        for et in &edge_types {
            let q = format!(
                "MATCH (s:Symbol)-[:{et}]->(t:Symbol) \
                 WHERE t.repo_uid = $repo AND t.file_path = $fp AND s.file_path <> $fp \
                 RETURN DISTINCT s.file_path"
            );
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![
                        ("repo", Value::String(repo_uid.to_string())),
                        ("fp", Value::String(file_path.to_string())),
                    ],
                )
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            for row in result {
                let path = extract_string(&row, 0)?;
                if !path.is_empty() {
                    files.insert(path);
                }
            }
        }
        Ok(files)
    }

    /// Returns all Symbol nodes that have an IMPORTS edge pointing TO `uid`.
    /// Used by impact analysis for SymbolRenamed changes.
    pub fn importers_of(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol)-[:IMPORTS]->(t:Symbol {{uid: $uid}}) RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Returns all CROSS_REPO_LINK edges in the graph, up to `limit` rows.
    ///
    /// Each row includes the source and target symbol names plus their repo UIDs
    /// so callers can group by (source_repo_uid, target_repo_uid) without
    /// performing additional lookups. Capped at 200 to avoid flooding a guide.
    pub fn list_all_cross_repo_links(&self, limit: usize) -> Result<Vec<CrossRepoRef>, StoreError> {
        let conn = self.conn()?;
        // KuzuDB does not support LIMIT with a bound parameter — embed the
        // integer directly (safe: it's a usize from caller code, not user input).
        let q = format!(
            "MATCH (s:Symbol)-[r:CROSS_REPO_LINK]->(t:Symbol) \
             RETURN s.uid, s.name, s.repo_uid, t.uid, t.name, t.repo_uid, r.link_type, r.confidence \
             LIMIT {limit}"
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result
            .map(|row| {
                let source_uid = extract_string(&row, 0)?;
                let source_name = extract_string(&row, 1)?;
                let source_repo_uid = extract_string(&row, 2)?;
                let target_uid = extract_string(&row, 3)?;
                let target_name = extract_string(&row, 4)?;
                let target_repo_uid = extract_string(&row, 5)?;
                let link_type = extract_string(&row, 6)?;
                let confidence = extract_f64(&row, 7)? as f32;
                Ok(CrossRepoRef {
                    source_uid,
                    source_name,
                    source_repo_uid: Some(source_repo_uid),
                    target_uid,
                    target_name,
                    target_repo_uid: Some(target_repo_uid),
                    link_type,
                    confidence,
                })
            })
            .collect()
    }

    /// Returns cross-repo link edges originating from or pointing to `uid`.
    pub fn cross_repo_links(&self, uid: &str) -> Result<Vec<CrossRepoRef>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (s:Symbol {uid: $uid})-[r:CROSS_REPO_LINK]->(t:Symbol) \
             RETURN s.uid, s.name, t.uid, t.name, r.link_type, r.confidence";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result
            .map(|row| {
                let source_uid = extract_string(&row, 0)?;
                let source_name = extract_string(&row, 1)?;
                let target_uid = extract_string(&row, 2)?;
                let target_name = extract_string(&row, 3)?;
                let link_type = extract_string(&row, 4)?;
                let confidence = extract_f64(&row, 5)? as f32;
                Ok(CrossRepoRef {
                    source_uid,
                    source_name,
                    source_repo_uid: None,
                    target_uid,
                    target_name,
                    target_repo_uid: None,
                    link_type,
                    confidence,
                })
            })
            .collect()
    }

    /// Returns (uid, kind) tuples for all symbols with `name` in the given repo.
    /// Lighter than `lookup_symbols_by_name` when the repo scope is known.
    pub fn lookup_symbols_by_name_in_repo(
        &self,
        name: &str,
        repo_uid: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (s:Symbol) WHERE s.name = $name AND s.repo_uid = $repo \
                 RETURN s.uid, s.kind";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![
                    ("name", Value::String(name.to_string())),
                    ("repo", Value::String(repo_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        let mut rows = Vec::new();
        for row in result {
            let uid = extract_string(&row, 0)?;
            let kind = extract_string(&row, 1)?;
            rows.push((uid, kind));
        }
        Ok(rows)
    }

    /// Returns (uid, name, kind) tuples for all symbols belonging to `repo_uid`.
    /// Lighter than `lookup_symbols_by_repo` — skips file_path, signature, etc.
    /// Used by cross-repo symbol-level matching in the engine.
    pub fn symbol_lite_by_repo(
        &self,
        repo_uid: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (s:Symbol) WHERE s.repo_uid = $repo RETURN s.uid, s.name, s.kind";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("repo", Value::String(repo_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        let mut rows = Vec::new();
        for row in result {
            let uid = extract_string(&row, 0)?;
            let name = extract_string(&row, 1)?;
            let kind = extract_string(&row, 2)?;
            rows.push((uid, name, kind));
        }
        Ok(rows)
    }

    /// Returns just the symbol names for a given repo — lighter than loading full Symbol objects.
    pub fn symbol_names_by_repo(&self, repo_uid: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (s:Symbol) WHERE s.repo_uid = $repo RETURN s.name";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("repo", Value::String(repo_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        Ok(result
            .filter_map(|row| match row.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    /// Returns all Symbol nodes that belong to the given repo (by `repo_uid`).
    pub fn lookup_symbols_by_repo(&self, repo_uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.repo_uid = $repo RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("repo", Value::String(repo_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Batch primary-key-oriented symbol hydration for derived-index hits.
    pub fn lookup_symbols_by_uids(&self, uids: &[String]) -> Result<Vec<Symbol>, StoreError> {
        // nw-141: drive this through the primary-key index, NOT a filtered scan.
        // A disjunction of equality predicates ("uid = $a OR uid = $b OR ...")
        // is one non-EQUALS predicate to the planner, so it is never rewritten
        // into a PRIMARY_KEY_SCAN and each chunk degenerates to a full table
        // scan — cost tracked table size rather than result size (16.9s to
        // hydrate from a 193k-row Symbol table for a <1KB result).
        //
        // UNWIND + a property-map match binds to a single EQUALS per element,
        // which IS index-accelerated: N probes inside one query plan. This
        // mirrors batch_lookup_symbols_impl above, which also documents the
        // correctness reason — filtered scans can return garbled non-PK string
        // values after delete+checkpoint cycles, while PK point lookups do not.
        const CHUNK: usize = 256;
        let conn = self.conn()?;
        let mut symbols = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let in_list: String = chunk
                .iter()
                .map(|u| format!("'{}'", u.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            let query = format!(
                "UNWIND [{}] AS want_uid \
                 MATCH (s:Symbol {{uid: want_uid}}) RETURN {}",
                in_list, SYMBOL_COLUMNS
            );
            let rows = conn
                .query(&query)
                .map_err(|error| StoreError::Query(format!("symbol UID batch: {error}")))?;
            symbols.extend(
                rows.map(|row| row_to_symbol(&row))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(symbols)
    }

    /// Like [`lookup_symbols_by_repo`] but on an externally-provided connection
    /// (for use inside an open transaction).
    pub fn lookup_symbols_by_repo_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
    ) -> Result<Vec<Symbol>, StoreError> {
        let safe_repo = repo_uid.replace('\'', "\\'");
        let rows = conn
            .query(&format!(
                "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo}' RETURN {SYMBOL_COLUMNS}"
            ))
            .map_err(|e| StoreError::Query(format!("lookup_symbols_by_repo_on: {e}")))?;
        rows.map(|row| row_to_symbol(&row)).collect()
    }

    /// Returns all Symbol nodes whose `file_path` property matches `file_path`.
    pub fn symbols_in_file(&self, file_path: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.file_path = $path RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("path", Value::String(file_path.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Symbols in `file_path` scoped to a single repo. In a unified multi-repo
    /// graph, identical relative paths (e.g. `src/main.rs`) exist in many repos;
    /// this avoids conflating them by also matching `repo_uid`.
    pub fn symbols_in_file_in_repo(
        &self,
        file_path: &str,
        repo_uid: &str,
    ) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.file_path = $path AND s.repo_uid = $repo RETURN {}",
            SYMBOL_COLUMNS
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![
                    ("path", Value::String(file_path.to_string())),
                    ("repo", Value::String(repo_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    // ── Brain extension: markdown node reads ────────────────────────────────

    /// List all Vault nodes, optionally filtered by instance ID.
    pub fn list_vaults(&self, instance_id: Option<&str>) -> Result<Vec<Vault>, StoreError> {
        let conn = self.conn()?;
        let cols = "v.uid, v.name, v.root_path, v.instance_id";
        let result = if let Some(iid) = instance_id {
            let q = format!("MATCH (v:Vault) WHERE v.instance_id = $iid RETURN {cols}");
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            conn.execute(&mut stmt, vec![("iid", Value::String(iid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?
        } else {
            let q = format!("MATCH (v:Vault) RETURN {cols}");
            conn.query(&q)
                .map_err(|e| StoreError::Query(e.to_string()))?
        };
        result
            .map(|row| {
                Ok(Vault {
                    uid: extract_string(&row, 0)?,
                    name: extract_string(&row, 1)?,
                    root_path: extract_string(&row, 2)?,
                    instance_id: extract_string(&row, 3)?,
                })
            })
            .collect()
    }

    /// List all Note nodes, optionally filtered by vault UID.
    pub fn list_notes(&self, vault_uid: Option<&str>) -> Result<Vec<Note>, StoreError> {
        let conn = self.conn()?;
        let result = if let Some(vid) = vault_uid {
            let q = format!("MATCH (n:Note) WHERE n.vault_uid = $vid RETURN {NOTE_COLUMNS}");
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            conn.execute(&mut stmt, vec![("vid", Value::String(vid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?
        } else {
            let q = format!("MATCH (n:Note) RETURN {NOTE_COLUMNS}");
            conn.query(&q)
                .map_err(|e| StoreError::Query(e.to_string()))?
        };
        result.map(|row| row_to_note(&row)).collect()
    }

    /// Count of all Note nodes (cheap for status output — no body load).
    pub fn count_notes(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (n:Note) RETURN n.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
    }

    /// Return all headings in a given note, in document order (by start_line).
    pub fn headings_in_note(&self, note_uid: &str) -> Result<Vec<Heading>, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (h:Heading) WHERE h.note_uid = $nid RETURN {HEADING_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("nid", Value::String(note_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        let mut headings: Vec<Heading> = result
            .map(|row| row_to_heading(&row))
            .collect::<Result<_, _>>()?;
        headings.sort_by_key(|h| h.start_line);
        Ok(headings)
    }

    /// Return all sections in a given note, in document order (by start_line).
    pub fn sections_in_note(&self, note_uid: &str) -> Result<Vec<Section>, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Section) WHERE s.note_uid = $nid RETURN {SECTION_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("nid", Value::String(note_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        let mut sections: Vec<Section> = result
            .map(|row| row_to_section(&row))
            .collect::<Result<_, _>>()?;
        sections.sort_by_key(|s| s.start_line);
        Ok(sections)
    }

    /// Batch primary-key-oriented section hydration for derived-index hits.
    pub fn lookup_sections_by_uids(&self, uids: &[String]) -> Result<Vec<Section>, StoreError> {
        // nw-141: primary-key probes via UNWIND, not a disjunction of equality
        // predicates. An OR chain is a single non-EQUALS predicate to the
        // planner, so it is never rewritten into a PRIMARY_KEY_SCAN and each
        // chunk degenerates to a full table scan. See lookup_symbols_by_uids.
        const CHUNK: usize = 256;
        let conn = self.conn()?;
        let mut sections = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let in_list: String = chunk
                .iter()
                .map(|u| format!("'{}'", u.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            let query = format!(
                "UNWIND [{}] AS want_uid \
                 MATCH (s:Section {{uid: want_uid}}) RETURN {}",
                in_list, SECTION_COLUMNS
            );
            let rows = conn
                .query(&query)
                .map_err(|error| StoreError::Query(format!("section UID batch: {error}")))?;
            sections.extend(
                rows.map(|row| row_to_section(&row))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(sections)
    }

    /// List all Heading nodes across all vaults/notes.
    /// Used by the brain_search substring fallback to extend title-only search
    /// to heading text.
    pub fn list_all_headings(&self) -> Result<Vec<Heading>, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (h:Heading) RETURN {HEADING_COLUMNS}");
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        result.map(|row| row_to_heading(&row)).collect()
    }

    /// List all Section nodes across all vaults/notes (includes `text_content`).
    /// Used by the brain_search substring fallback to search section bodies.
    pub fn list_all_sections(&self) -> Result<Vec<Section>, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Section) RETURN {SECTION_COLUMNS}");
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        result.map(|row| row_to_section(&row)).collect()
    }

    /// List all Heading nodes belonging to notes in the given vault.
    pub fn list_headings_by_vault(&self, vault_uid: &str) -> Result<Vec<Heading>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (n:Note {{vault_uid: $vid}})-[:NOTE_HAS_HEADING]->(h:Heading) RETURN {HEADING_COLUMNS}"
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("vid", Value::String(vault_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_heading(&row)).collect()
    }

    /// List all Section nodes belonging to notes in the given vault.
    pub fn list_sections_by_vault(&self, vault_uid: &str) -> Result<Vec<Section>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (n:Note {{vault_uid: $vid}})-[:NOTE_HAS_SECTION]->(s:Section) RETURN {SECTION_COLUMNS}"
        );
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("vid", Value::String(vault_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_section(&row)).collect()
    }

    /// Count of all Heading nodes — cheap status check.
    pub fn count_headings(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (h:Heading) RETURN h.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
    }

    /// Count of all Section nodes — cheap status check.
    pub fn count_sections(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (s:Section) RETURN s.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
    }

    /// List all Tag nodes, optionally filtered by vault UID.
    pub fn list_tags(&self, vault_uid: Option<&str>) -> Result<Vec<Tag>, StoreError> {
        let conn = self.conn()?;
        let cols = "t.uid, t.vault_uid, t.name";
        let result = if let Some(vid) = vault_uid {
            let q = format!("MATCH (t:Tag) WHERE t.vault_uid = $vid RETURN {cols}");
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            conn.execute(&mut stmt, vec![("vid", Value::String(vid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?
        } else {
            let q = format!("MATCH (t:Tag) RETURN {cols}");
            conn.query(&q)
                .map_err(|e| StoreError::Query(e.to_string()))?
        };
        result
            .map(|row| {
                Ok(Tag {
                    uid: extract_string(&row, 0)?,
                    vault_uid: extract_string(&row, 1)?,
                    name: extract_string(&row, 2)?,
                })
            })
            .collect()
    }

    /// Look up a single Tag by UID.
    pub fn lookup_tag(&self, uid: &str) -> Result<Tag, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (t:Tag {uid: $uid}) RETURN t.uid, t.vault_uid, t.name";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => Ok(Tag {
                uid: extract_string(&row, 0)?,
                vault_uid: extract_string(&row, 1)?,
                name: extract_string(&row, 2)?,
            }),
            None => Err(StoreError::NotFound),
        }
    }

    /// Count of all Tag nodes.
    pub fn count_tags(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (t:Tag) RETURN t.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
    }

    /// Total count of Symbol nodes in the graph.
    pub fn count_symbols(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (s:Symbol) RETURN s.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
    }

    /// Return the UIDs of every Symbol currently attached to the given files.
    ///
    /// Liveness scoped to exactly the files a run touched, for targeted
    /// embedding reconciliation. Deliberately NOT repo-scoped: the code
    /// watcher runs this on every save, and a repo-wide symbol scan on a large
    /// repo would put a six-figure row count in the path of every keystroke-
    /// triggered re-index. One query per touched file mirrors what
    /// `delete_symbols_in_file_on` already pays, so the added cost is a
    /// constant factor on work the run does anyway.
    ///
    /// Sound because the file set is exactly the set the caller deleted from:
    /// a UID can only reappear in a file the run wrote, and the rename path
    /// touches both the source and destination paths.
    /// The Note and Heading UIDs owned by `note_uid` that currently carry an
    /// embedding candidate — collected BEFORE a cascade delete so the caller
    /// can tombstone whatever does not come back.
    ///
    /// Mirrors `delete_note_cascade_on`'s own two passes: ownership by the
    /// `note_uid` PROPERTY and by the `NOTE_HAS_SECTION`/`NOTE_HAS_HEADING`
    /// EDGE. The cascade runs both because the edge is not guaranteed, so
    /// collecting by only one of them would miss exactly the fragments the
    /// cascade exists to catch.
    pub fn note_embedding_candidate_uids(&self, note_uid: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        // Same `\'` escaping as `symbol_uids_for_files`: these queries and the
        // cascade's must select the SAME rows for the liveness difference to
        // be sound.
        let safe = note_uid.replace('\'', "\\'");
        let mut uids = vec![note_uid.to_string()];
        for query in [
            format!("MATCH (h:Heading) WHERE h.note_uid = '{safe}' RETURN h.uid"),
            format!(
                "MATCH (n:Note {{uid: '{safe}'}})-[:NOTE_HAS_HEADING]->(h:Heading) RETURN h.uid"
            ),
        ] {
            let Ok(mut stmt) = conn.prepare(&query) else {
                // A schema without that edge type is not an error here; the
                // property pass still covers ownership.
                continue;
            };
            let rows = conn
                .execute(&mut stmt, vec![])
                .map_err(|e| StoreError::Query(format!("note embedding candidates: {e}")))?;
            for row in rows {
                if let Some(Value::String(uid)) = row.first() {
                    uids.push(uid.clone());
                }
            }
        }
        uids.sort();
        uids.dedup();
        Ok(uids)
    }

    /// Which of `candidates` still exist as a Note or Heading.
    pub fn live_vault_node_uids(
        &self,
        candidates: &[String],
    ) -> Result<HashSet<String>, StoreError> {
        let mut live = HashSet::new();
        if candidates.is_empty() {
            return Ok(live);
        }
        let conn = self.conn()?;
        for candidate in candidates {
            let safe = candidate.replace('\'', "\\'");
            for label in ["Note", "Heading"] {
                let query =
                    format!("MATCH (n:{label}) WHERE n.uid = '{safe}' RETURN n.uid LIMIT 1");
                let Ok(mut stmt) = conn.prepare(&query) else {
                    continue;
                };
                let rows = conn
                    .execute(&mut stmt, vec![])
                    .map_err(|e| StoreError::Query(format!("live vault nodes: {e}")))?;
                if rows.into_iter().next().is_some() {
                    live.insert(candidate.clone());
                    break;
                }
            }
        }
        Ok(live)
    }

    pub fn symbol_uids_for_files(
        &self,
        repo_uid: &str,
        file_paths: &[String],
    ) -> Result<HashSet<String>, StoreError> {
        let mut uids = HashSet::new();
        if file_paths.is_empty() {
            return Ok(uids);
        }
        let conn = self.conn()?;
        // LadybugDB does not support parameterized compound WHERE clauses.
        // Escaping deliberately mirrors `delete_symbols_in_file_on` (`\'`), NOT
        // the SQL-style `''` doubling used elsewhere in this file: these two
        // queries must select the SAME rows for the liveness difference to be
        // sound, so they have to agree with each other rather than with the
        // file. That the codebase carries two escaping conventions is a real
        // inconsistency and its own cleanup.
        let safe_repo_uid = repo_uid.replace('\'', "\\'");
        for file_path in file_paths {
            let safe_file_path = file_path.replace('\'', "\\'");
            let result = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo_uid}' AND s.file_path = '{safe_file_path}' RETURN s.uid"
                ))
                .map_err(|e| StoreError::Query(format!("list symbol uids in file: {e}")))?;
            for row in result {
                uids.insert(extract_string(&row, 0)?);
            }
        }
        Ok(uids)
    }

    /// Return the authoritative set of graph-node UIDs supported by the
    /// sidecar embedding index. The embedding producers currently cover
    /// Symbols, Notes, and Headings; querying all three protects vault data
    /// while code-repo deletion reconciles Symbol vectors.
    pub(crate) fn live_embedding_node_uids(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let mut uids = HashSet::new();
        for query in [
            "MATCH (s:Symbol) RETURN s.uid",
            "MATCH (n:Note) RETURN n.uid",
            "MATCH (h:Heading) RETURN h.uid",
        ] {
            let result = conn
                .query(query)
                .map_err(|e| StoreError::Query(e.to_string()))?;
            for row in result {
                uids.insert(extract_string(&row, 0)?);
            }
        }
        Ok(uids)
    }

    /// Return every UID-bearing graph node currently present. This is the
    /// authoritative liveness set for UID-keyed external sidecars.
    pub fn live_graph_node_uids(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let mut uids = HashSet::new();
        for query in [
            "MATCH (n:Repo) RETURN n.uid",
            "MATCH (n:File) RETURN n.uid",
            "MATCH (n:Service) RETURN n.uid",
            "MATCH (n:Symbol) RETURN n.uid",
            "MATCH (n:Vault) RETURN n.uid",
            "MATCH (n:Note) RETURN n.uid",
            "MATCH (n:Heading) RETURN n.uid",
            "MATCH (n:Section) RETURN n.uid",
            "MATCH (n:Tag) RETURN n.uid",
            "MATCH (n:Project) RETURN n.uid",
            "MATCH (n:Contract) RETURN n.uid",
            "MATCH (n:UnresolvedWikilink) RETURN n.uid",
        ] {
            let result = conn.query(query).map_err(|error| {
                StoreError::Query(format!(
                    "graph UID liveness query failed ({query}): {error}"
                ))
            })?;
            for row in result {
                uids.insert(extract_string(&row, 0)?);
            }
        }
        Ok(uids)
    }

    /// Count symbols grouped by their owning repo (`repo_uid` -> count).
    /// Used by backup manifests to report per-repo symbol totals without
    /// loading full symbol rows.
    pub fn count_symbols_by_repo(
        &self,
    ) -> Result<std::collections::HashMap<String, usize>, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (s:Symbol) RETURN s.repo_uid, count(s.uid)")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut counts = std::collections::HashMap::new();
        for row in result {
            let repo_uid = extract_string(&row, 0)?;
            let count = extract_i64(&row, 1).unwrap_or(0).max(0) as usize;
            counts.insert(repo_uid, count);
        }
        Ok(counts)
    }

    /// Returns true when the repo has any indexed content — an existence
    /// probe (`LIMIT 1`) used to detect repos whose indexed SHA was committed
    /// but whose content never landed (interrupted index).
    ///
    /// Two legs, matching what each index path writes:
    /// - Code index: `File` nodes keyed by `repo_uid` are written for EVERY
    ///   parsed file regardless of symbol count (`index.rs`), so a healthy
    ///   code repo — even one whose files yield zero symbols — always has
    ///   File rows; a crash before content landed has none.
    /// - Server-mode vault index: writes only Note/Section/Heading nodes
    ///   (linked `Vault -[:VAULT_HAS_NOTE]-> Note`; notes carry `vault_uid`,
    ///   no `repo_uid`) and records the SHA on a Repo row whose `url` equals
    ///   the Vault's `name` — both are the job's repo URL (`vault_name` in
    ///   `index_md.rs`). The name is matched with and without a trailing
    ///   slash: the Repo row stores the trimmed URL while `Vault.name` keeps
    ///   it verbatim.
    pub fn repo_has_content(&self, repo: &Repo) -> Result<bool, StoreError> {
        let conn = self.conn()?;
        // Leg 1: code content.
        let mut stmt = conn
            .prepare("MATCH (f:File) WHERE f.repo_uid = $repo RETURN f.uid LIMIT 1")
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("repo", Value::String(repo.uid.clone()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        if result.next().is_some() {
            return Ok(true);
        }
        // Leg 2: vault content (server-mode vault Repo rows).
        let trimmed = repo.url.trim_end_matches('/');
        let mut stmt = conn
            .prepare(
                "MATCH (v:Vault)-[:VAULT_HAS_NOTE]->(n:Note) \
                 WHERE v.instance_id = $inst AND (v.name = $url OR v.name = $url_slash) \
                 RETURN n.uid LIMIT 1",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![
                    ("inst", Value::String(repo.instance_id.clone())),
                    ("url", Value::String(trimmed.to_string())),
                    ("url_slash", Value::String(format!("{trimmed}/"))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        Ok(result.next().is_some())
    }

    /// True when the repo's indexed SHA was committed but no content ever
    /// landed (interrupted index) — the state the incremental self-heal and
    /// stale-check must flag. Shared so both stale-check call sites cannot
    /// drift on the predicate; errors propagate (a CI gate that cannot
    /// answer must fail, not silently pass).
    pub fn repo_index_incomplete(&self, repo: &Repo) -> Result<bool, StoreError> {
        Ok(!repo.indexed_sha.is_empty() && !self.repo_has_content(repo)?)
    }

    /// Returns the dimension of stored embeddings, or 0 if none exist.
    ///
    /// Checks the sidecar `EmbeddingIndex` (the authoritative source since
    /// lbug has no float-array columns). Returns 0 when the index is empty.
    pub fn embedding_dimension(&self) -> Result<u32, StoreError> {
        if let Some(dim) = self.embedding_index_dimension() {
            return Ok(u32::try_from(dim).unwrap_or(0));
        }
        Ok(0)
    }

    /// All symbols with full details including the embedding field.
    /// Used by vector KNN search to load embeddings for cosine similarity.
    pub fn list_all_symbols(&self) -> Result<Vec<nestweaver_schema::Symbol>, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Symbol) RETURN {}", SYMBOL_COLUMNS);
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// All symbols' (uid, name, kind_string). Lightweight — no signature
    /// or file_path loaded. Used by cross-domain link discovery to scan
    /// note bodies for symbol-name matches.
    pub fn list_all_symbols_lite(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (s:Symbol) RETURN s.uid, s.name, s.kind")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut rows = Vec::new();
        for row in result {
            let uid = extract_string(&row, 0)?;
            let name = extract_string(&row, 1)?;
            let kind = extract_string(&row, 2)?;
            rows.push((uid, name, kind));
        }
        Ok(rows)
    }

    /// Total count of REFERENCES_CODE edges (note-to-symbol + section-to-symbol).
    /// Useful for status output to confirm cross-domain discovery happened.
    pub fn count_references_code_edges(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let note_to_sym = conn
            .query("MATCH ()-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->() RETURN r")
            .map_err(|e| StoreError::Query(e.to_string()))?
            .count();
        let sec_to_sym = conn
            .query("MATCH ()-[r:REFERENCES_CODE_SECTION_TO_SYMBOL]->() RETURN r")
            .map_err(|e| StoreError::Query(e.to_string()))?
            .count();
        Ok(note_to_sym + sec_to_sym)
    }

    /// Count of all wikilink edges (to either Note or Heading). Cheap status
    /// summary — does two separate queries since LadybugDB splits the
    /// logical WIKILINK into two physical REL tables.
    pub fn count_wikilink_edges(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let to_notes = conn
            .query("MATCH ()-[r:WIKILINK_TO_NOTE]->() RETURN r")
            .map_err(|e| StoreError::Query(e.to_string()))?
            .count();
        let to_headings = conn
            .query("MATCH ()-[r:WIKILINK_TO_HEADING]->() RETURN r")
            .map_err(|e| StoreError::Query(e.to_string()))?
            .count();
        Ok(to_notes + to_headings)
    }

    /// Look up a single Vault by UID. Used by tools that need to translate
    /// a Note's vault-relative `file_path` into an on-disk absolute path.
    pub fn lookup_vault(&self, uid: &str) -> Result<Vault, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (v:Vault {uid: $uid}) RETURN v.uid, v.name, v.root_path, v.instance_id";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => Ok(Vault {
                uid: extract_string(&row, 0)?,
                name: extract_string(&row, 1)?,
                root_path: extract_string(&row, 2)?,
                instance_id: extract_string(&row, 3)?,
            }),
            None => Err(StoreError::NotFound),
        }
    }

    /// Look up notes by title (case-insensitive exact match).
    pub fn lookup_notes_by_title(&self, title: &str) -> Result<Vec<Note>, StoreError> {
        let all = self.list_notes(None)?;
        let needle = title.to_lowercase();
        Ok(all
            .into_iter()
            .filter(|n| n.title.to_lowercase() == needle)
            .collect())
    }

    /// Find every Note that links *to* `note_uid` via a WIKILINK_TO_NOTE
    /// edge or via a WIKILINK_TO_HEADING edge whose heading belongs to the
    /// target note. Returns the source note (the linker), the section it
    /// came from, and the link confidence + display string.
    pub fn wikilink_sources_to_note(&self, note_uid: &str) -> Result<Vec<BacklinkRow>, StoreError> {
        let conn = self.conn()?;
        let mut rows = Vec::new();

        // Path 1: direct WIKILINK_TO_NOTE edges.
        // Traverse: source Note ← NOTE_HAS_SECTION ← Section -[r:WIKILINK_TO_NOTE]→ target Note.
        let q1 = "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(s:Section)-[r:WIKILINK_TO_NOTE]->(n:Note {uid: $uid}) \
                  RETURN src.uid, src.title, src.file_path, s.uid, r.confidence, r.display";
        let mut stmt1 = conn
            .prepare(q1)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result1 = conn
            .execute(
                &mut stmt1,
                vec![("uid", Value::String(note_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        for row in result1 {
            let source_note_uid = extract_string(&row, 0)?;
            let source_note_title = extract_string(&row, 1)?;
            let source_note_path = extract_string(&row, 2)?;
            let source_section_uid = extract_string(&row, 3)?;
            let confidence = extract_f64(&row, 4)? as f32;
            let display = extract_opt_string(&row, 5)?;
            rows.push(BacklinkRow {
                source_note_uid,
                source_note_title,
                source_note_path,
                source_section_uid,
                confidence,
                display,
            });
        }

        // Path 2: WIKILINK_TO_HEADING edges whose heading belongs to the target note.
        // Traverse: source Note ← NOTE_HAS_SECTION ← Section -[r:WIKILINK_TO_HEADING]→ Heading ← NOTE_HAS_HEADING ← target Note.
        let q2 = "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(s:Section)-[r:WIKILINK_TO_HEADING]->(h:Heading {note_uid: $uid}) \
                  RETURN src.uid, src.title, src.file_path, s.uid, r.confidence, r.display";
        match conn.prepare(q2) {
            Ok(mut stmt2) => {
                let result2 = conn
                    .execute(
                        &mut stmt2,
                        vec![("uid", Value::String(note_uid.to_string()))],
                    )
                    .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
                let mut seen_sources: std::collections::HashSet<String> = rows
                    .iter()
                    .map(|r| format!("{}:{}", r.source_note_uid, r.source_section_uid))
                    .collect();
                for row in result2 {
                    let source_note_uid = extract_string(&row, 0)?;
                    let source_section_uid = extract_string(&row, 3)?;
                    let key = format!("{source_note_uid}:{source_section_uid}");
                    if seen_sources.contains(&key) {
                        continue; // Already found via direct note link.
                    }
                    seen_sources.insert(key);
                    let source_note_title = extract_string(&row, 1)?;
                    let source_note_path = extract_string(&row, 2)?;
                    let confidence = extract_f64(&row, 4)? as f32;
                    let display = extract_opt_string(&row, 5)?;
                    rows.push(BacklinkRow {
                        source_note_uid,
                        source_note_title,
                        source_note_path,
                        source_section_uid,
                        confidence,
                        display,
                    });
                }
            }
            Err(e) => {
                // WIKILINK_TO_HEADING table may not exist (no heading wikilinks indexed).
                tracing::debug!("wikilink_sources_to_note: heading path skipped: {e}");
            }
        }

        Ok(rows)
    }

    /// Look up a single note by UID.
    pub fn lookup_note(&self, uid: &str) -> Result<Note, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (n:Note {{uid: $uid}}) RETURN {NOTE_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => row_to_note(&row),
            None => Err(StoreError::NotFound),
        }
    }

    /// Batch primary-key-oriented note hydration for derived-index hits.
    pub fn lookup_notes_by_uids(&self, uids: &[String]) -> Result<Vec<Note>, StoreError> {
        // nw-141: primary-key probes via UNWIND, not a disjunction of equality
        // predicates. An OR chain is a single non-EQUALS predicate to the
        // planner, so it is never rewritten into a PRIMARY_KEY_SCAN and each
        // chunk degenerates to a full table scan. See lookup_symbols_by_uids.
        const CHUNK: usize = 256;
        let conn = self.conn()?;
        let mut notes = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let in_list: String = chunk
                .iter()
                .map(|u| format!("'{}'", u.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            let query = format!(
                "UNWIND [{}] AS want_uid \
                 MATCH (n:Note {{uid: want_uid}}) RETURN {}",
                in_list, NOTE_COLUMNS
            );
            let rows = conn
                .query(&query)
                .map_err(|error| StoreError::Query(format!("note UID batch: {error}")))?;
            notes.extend(
                rows.map(|row| row_to_note(&row))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(notes)
    }

    /// Look up a single Section by UID.
    pub fn lookup_section(&self, uid: &str) -> Result<Section, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Section {{uid: $uid}}) RETURN {SECTION_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => row_to_section(&row),
            None => Err(StoreError::NotFound),
        }
    }

    /// Look up a single Heading by UID.
    pub fn lookup_heading(&self, uid: &str) -> Result<Heading, StoreError> {
        let conn = self.conn()?;
        let q = format!("MATCH (h:Heading {{uid: $uid}}) RETURN {HEADING_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        match result.next() {
            Some(row) => row_to_heading(&row),
            None => Err(StoreError::NotFound),
        }
    }

    /// Returns every File node whose `repo_uid` matches `repo_uid`.
    /// Each row is `(file_uid, file_path)`.
    pub fn list_files_by_repo(&self, repo_uid: &str) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (f:File) WHERE f.repo_uid = $repo RETURN f.uid, f.path";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("repo", Value::String(repo_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result
            .map(|row| {
                let uid = extract_string(&row, 0)?;
                let path = extract_string(&row, 1)?;
                Ok((uid, path))
            })
            .collect()
    }

    /// Look up a single Repo by UID. Returns `None` if no such repo exists.
    pub fn lookup_repo(
        &self,
        repo_uid: &str,
    ) -> Result<Option<nestweaver_schema::Repo>, StoreError> {
        let repos = self.list_repos(None)?;
        Ok(repos.into_iter().find(|r| r.uid == repo_uid))
    }

    /// Returns the repo URL for a given repo UID, or an empty string if
    /// the repo is not found.
    pub fn repo_url_for_uid(&self, repo_uid: &str) -> Option<String> {
        self.lookup_repo(repo_uid).ok().flatten().map(|r| r.url)
    }

    /// Returns all Symbol nodes and code-level edges for clustering.
    pub fn load_code_symbols_and_edges(&self) -> Result<CodeGraph, StoreError> {
        let conn = self.conn()?;

        let q = "MATCH (s:Symbol) RETURN s.uid, s.name, s.file_path, s.kind, s.repo_uid";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut symbols = Vec::new();
        for row in result {
            let uid = extract_string(&row, 0)?;
            let name = extract_string(&row, 1)?;
            let file_path = extract_string(&row, 2)?;
            let kind = extract_string(&row, 3)?;
            let repo_uid = extract_string(&row, 4)?;
            symbols.push(SymbolBasic {
                uid,
                name,
                file_path,
                kind,
                repo_uid,
            });
        }

        let edge_types: Vec<&str> = nestweaver_schema::ALL_SYMBOL_EDGE_TYPES
            .iter()
            .map(|et| et.rel_table_name())
            .collect();
        let mut edges: Vec<(String, String, f64)> = Vec::new();
        for et in &edge_types {
            let q =
                format!("MATCH (a:Symbol)-[r:{et}]->(b:Symbol) RETURN a.uid, b.uid, r.confidence");
            let result = match conn.query(&q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!(
                        "load_code_symbols_and_edges: edge type {et} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            for row in result {
                let src = extract_string(&row, 0)?;
                let dst = extract_string(&row, 1)?;
                let confidence = extract_f64(&row, 2)?;
                edges.push((src, dst, confidence));
            }
        }

        Ok((symbols, edges))
    }

    /// Returns all code-level edges with their type label, confidence, and evidence.
    ///
    /// Each tuple is `(source_uid, target_uid, edge_type, confidence, evidence)`.
    /// Used by graph-export functions that need the relationship type.
    pub fn load_typed_edges(&self) -> Result<Vec<TypedEdge>, StoreError> {
        let conn = self.conn()?;

        let edge_types: Vec<&str> = nestweaver_schema::ALL_SYMBOL_EDGE_TYPES
            .iter()
            .map(|et| et.rel_table_name())
            .collect();
        let mut edges: Vec<(String, String, String, f64, String)> = Vec::new();
        for et in &edge_types {
            let q = format!(
                "MATCH (a:Symbol)-[r:{et}]->(b:Symbol) RETURN a.uid, b.uid, r.confidence, r.evidence"
            );
            let result = match conn.query(&q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!(
                        "load_typed_edges: edge type {et} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            for row in result {
                let src = extract_string(&row, 0)?;
                let dst = extract_string(&row, 1)?;
                let confidence = extract_f64(&row, 2)?;
                let evidence = extract_string(&row, 3).unwrap_or_default();
                edges.push((src, dst, et.to_string(), confidence, evidence));
            }
        }

        // FILE_HAS_SYMBOL (DEFINES) edges: File → Symbol
        let q = "MATCH (f:File)-[r:FILE_HAS_SYMBOL]->(s:Symbol) RETURN f.uid, s.uid";
        if let Ok(result) = conn.query(q) {
            for row in result {
                let src = extract_string(&row, 0)?;
                let dst = extract_string(&row, 1)?;
                edges.push((src, dst, "DEFINES".to_string(), 1.0, String::new()));
            }
        }

        Ok(edges)
    }

    /// Returns all Symbol nodes that `uid` calls or imports (outgoing edges).
    ///
    /// Follows CALLS, IMPORTS, and CROSS_REPO_LINK edges so that
    /// `flow_trace` can traverse function calls, import relationships,
    /// and cross-repo boundaries.
    ///
    /// Prefer [`callees_with_edge_types_of`](Self::callees_with_edge_types_of)
    /// for anything user-facing: this drops the edge type, and a CROSS_REPO_LINK
    /// is a cross-repo HYPOTHESIS, not an observed call.
    pub fn callees_of(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        Ok(self
            .callees_with_edge_types_of(uid)?
            .into_iter()
            .map(|(sym, _)| sym)
            .collect())
    }

    /// As [`callees_of`](Self::callees_of), but keeps the edge type that reached
    /// each callee.
    ///
    /// The traversal spans three very different relationships, and collapsing
    /// them was the defect: a CROSS_REPO_LINK is an INFERRED link between repos,
    /// so following it as though it were a call produced fabricated execution
    /// paths — tracing a Rust function returned JavaScript symbols from unrelated
    /// repos as its callees. Because the edge type was discarded here,
    /// `flow_trace` could not label them and a consumer had no way to tell a real
    /// call from a cross-repo guess. `impact` already returns the edge type for
    /// exactly this reason (nw-111).
    ///
    /// Returned in edge-type priority order, and de-duplicated by symbol, so a
    /// callee reachable by both CALLS and CROSS_REPO_LINK is reported as the
    /// stronger CALLS.
    pub fn callees_with_edge_types_of(
        &self,
        uid: &str,
    ) -> Result<Vec<(Symbol, String)>, StoreError> {
        let conn = self.conn()?;
        let cols = SYMBOL_COLUMNS.replace("s.", "t.");
        // Order matters: strongest evidence first, so the de-dup below keeps it.
        let edge_types = ["CALLS", "IMPORTS", "CROSS_REPO_LINK"];
        let mut all: Vec<(Symbol, String)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for et in &edge_types {
            let q = format!("MATCH (s:Symbol {{uid: $uid}})-[:{et}]->(t:Symbol) RETURN {cols}");
            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            let result = conn
                .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            for row in result {
                let sym = row_to_symbol(&row)?;
                if seen.insert(sym.uid.clone()) {
                    all.push((sym, (*et).to_string()));
                }
            }
        }
        Ok(all)
    }

    /// Returns direct members of a class/container via MEMBER_OF edges.
    pub fn members_of(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let cols = SYMBOL_COLUMNS.replace("s.", "t.");
        let q = format!("MATCH (t:Symbol)-[:MEMBER_OF]->(s:Symbol {{uid: $uid}}) RETURN {cols}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Expand each requested tag to itself plus every nested descendant.
    ///
    /// nw-172: `#project` matched only the literal tag `project`, so a filter
    /// on a PARENT that exists in the hierarchy returned zero — indistinguishable
    /// from "no such tag" — while `project/nestweaver` returned 46. A silent
    /// zero for a tag that demonstrably has children is the defect.
    ///
    /// Descendants-by-default matches what Obsidian's own search does for a
    /// parent tag, which is the behaviour a vault user arrives with. (Obsidian
    /// is not uniformly consistent here — its Bases `tags.contains()` is
    /// exact-only, and that inconsistency is an active complaint — so the
    /// argument is the silent zero, not "Obsidian does it".)
    ///
    /// Separator is `/`, matching the nested-tag syntax. `project` expands to
    /// `project` and `project/*`, but never to `projects` — a prefix match
    /// without the separator would silently widen the filter to unrelated tags.
    pub fn expand_tags_with_descendants(
        &self,
        tag_names: &[String],
    ) -> Result<Vec<String>, StoreError> {
        if tag_names.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.list_tags(None)?;
        let mut expanded: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for requested in tag_names {
            // Tag names are stored LOWERCASED (`index_md.rs` canonicalizes at
            // both construction sites), so a raw comparison silently misses
            // every mixed-case request: a note tagged `#Project/NestWeaver` is
            // stored as `project/nestweaver`, and `tags=["Project"]` matched
            // nothing while `brain_tag_graph {"tag":"Project"}` — which
            // lowercases — reported it. Same rule, opposite answers.
            let requested = requested.trim_start_matches('#').to_lowercase();
            let requested = requested.as_str();
            let prefix = format!("{requested}/");
            for tag in &all {
                if (tag.name == requested || tag.name.starts_with(&prefix))
                    && seen.insert(tag.name.clone())
                {
                    expanded.push(tag.name.clone());
                }
            }
            // A requested tag that matches nothing at all is still passed
            // through, so the caller's own "no such tag" handling still fires
            // rather than being masked by an empty expansion.
            if seen.insert(requested.to_string()) {
                expanded.push(requested.to_string());
            }
        }
        Ok(expanded)
    }

    /// Returns the set of Note UIDs that are tagged with any of the given tag
    /// names, INCLUDING their nested descendants (see
    /// [`expand_tags_with_descendants`]).
    ///
    /// [`expand_tags_with_descendants`]: Self::expand_tags_with_descendants
    pub fn list_note_uids_with_tags(
        &self,
        tag_names: &[String],
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let tag_names = &self.expand_tags_with_descendants(tag_names)?;
        let conn = self.conn()?;
        let mut uids = std::collections::HashSet::new();
        for tag_name in tag_names {
            let q = "MATCH (n:Note)-[:NOTE_TAGGED_WITH]->(t:Tag {name: $name}) RETURN n.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(e.to_string()))?;
            let result = conn
                .execute(&mut stmt, vec![("name", Value::String(tag_name.clone()))])
                .map_err(|e| StoreError::Query(e.to_string()))?;
            for row in result {
                if let Some(Value::String(uid)) = row.first() {
                    uids.insert(uid.clone());
                }
            }
        }
        Ok(uids)
    }

    /// Returns the set of Section UIDs that are tagged with any of the given
    /// tag names, INCLUDING their nested descendants.
    pub fn list_section_uids_with_tags(
        &self,
        tag_names: &[String],
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let tag_names = &self.expand_tags_with_descendants(tag_names)?;
        let conn = self.conn()?;
        let mut uids = std::collections::HashSet::new();
        for tag_name in tag_names {
            let q = "MATCH (s:Section)-[:SECTION_TAGGED_WITH]->(t:Tag {name: $name}) RETURN s.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(e.to_string()))?;
            let result = conn
                .execute(&mut stmt, vec![("name", Value::String(tag_name.clone()))])
                .map_err(|e| StoreError::Query(e.to_string()))?;
            for row in result {
                if let Some(Value::String(uid)) = row.first() {
                    uids.insert(uid.clone());
                }
            }
        }
        Ok(uids)
    }

    /// Returns the set of Note UIDs whose `modified_at` is >= `since` (ISO 8601 string).
    /// If the Note table doesn't exist yet, returns an empty set (trace-level log).
    pub fn list_note_uids_modified_since(
        &self,
        since: &str,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (n:Note) WHERE n.modified_at >= $since RETURN n.uid";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!(
                    "list_note_uids_modified_since: query skipped (table may not exist): {e}"
                );
                return Ok(std::collections::HashSet::new());
            }
        };
        // nw-295. A failed query and "nothing has been modified since then"
        // are different facts and used to be the same value — the execute
        // error was swallowed into `Ok(HashSet::new())`, so a broken query
        // presented as a confidently narrowed result and the caller lost
        // every Note and Section without being told why. The `prepare`
        // fallback above is different and stays: a Note table that does not
        // exist means there genuinely are no notes.
        let result = conn.execute(&mut stmt, vec![("since", Value::String(since.to_string()))])?;
        let mut uids = std::collections::HashSet::new();
        for row in result {
            if let Some(Value::String(uid)) = row.first() {
                uids.insert(uid.clone());
            }
        }
        Ok(uids)
    }

    /// Returns the set of Section UIDs whose parent Note has `modified_at` >= `since`.
    /// Joins through Note → Section via `note_uid`. If the tables don't exist, returns empty set.
    pub fn list_section_uids_modified_since(
        &self,
        since: &str,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        // Get the note UIDs first, then collect all section UIDs for those notes.
        let note_uids = self.list_note_uids_modified_since(since)?;
        if note_uids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let conn = self.conn()?;
        let mut uids = std::collections::HashSet::new();
        for note_uid in &note_uids {
            let q = "MATCH (s:Section) WHERE s.note_uid = $nid RETURN s.uid";
            let mut stmt = match conn.prepare(q) {
                Ok(s) => s,
                Err(e) => {
                    tracing::trace!(
                        "list_section_uids_modified_since: query skipped (table may not exist): {e}"
                    );
                    return Ok(uids);
                }
            };
            // Same argument one layer down: the `continue` this replaces
            // silently dropped one note's sections from the answer.
            let result = conn.execute(&mut stmt, vec![("nid", Value::String(note_uid.clone()))])?;
            for row in result {
                if let Some(Value::String(uid)) = row.first() {
                    uids.insert(uid.clone());
                }
            }
        }
        Ok(uids)
    }

    /// Returns the set of all symbol UIDs that are the target of at least one
    /// CALLS edge. Single bulk query instead of per-symbol lookups.
    pub fn all_callee_uids(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (:Symbol)-[:CALLS]->(t:Symbol) RETURN DISTINCT t.uid";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut uids = std::collections::HashSet::new();
        for row in result {
            if let Ok(uid) = extract_string(&row, 0) {
                uids.insert(uid);
            }
        }
        Ok(uids)
    }

    /// List all Project nodes.
    pub fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (p:Project) RETURN p.uid, p.name, p.summary, p.instance_id";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        result
            .map(|row| {
                Ok(Project {
                    uid: extract_string(&row, 0)?,
                    name: extract_string(&row, 1)?,
                    summary: extract_opt_string(&row, 2)?,
                    instance_id: extract_string(&row, 3)?,
                })
            })
            .collect()
    }

    /// Return whether a Project UID is currently present in the graph.
    ///
    /// This narrow lookup is used to resolve an ambiguous delete transaction
    /// before removing UID-scoped sidecar metadata.
    pub fn project_exists(&self, project_uid: &str) -> Result<bool, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("MATCH (p:Project {uid: $uid}) RETURN p.uid")
            .map_err(|error| StoreError::Query(format!("prepare Project liveness: {error}")))?;
        let mut rows = conn
            .execute(
                &mut stmt,
                vec![("uid", lbug::Value::String(project_uid.to_string()))],
            )
            .map_err(|error| StoreError::Query(format!("execute Project liveness: {error}")))?;
        Ok(rows.next().is_some())
    }

    /// List all Contract nodes, optionally filtered to a single repo.
    pub fn list_contracts(&self, repo_uid: Option<&str>) -> Result<Vec<Contract>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (c:Contract) RETURN c.uid, c.kind, c.verb, c.path, \
                 c.operation_id, c.repo_uid, c.source_path, c.confidence";
        let result = match conn.query(q) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!("list_contracts: query skipped (table may not exist): {e}");
                return Ok(vec![]);
            }
        };
        let mut out: Vec<Contract> = Vec::new();
        for row in result {
            let c = Contract {
                uid: extract_string(&row, 0)?,
                kind: extract_string(&row, 1)?,
                verb: extract_opt_string(&row, 2)?,
                path: extract_opt_string(&row, 3)?,
                operation_id: extract_opt_string(&row, 4)?,
                repo_uid: extract_string(&row, 5)?,
                source_path: extract_string(&row, 6)?,
                confidence: extract_f64(&row, 7)? as f32,
            };
            if let Some(want) = repo_uid
                && c.repo_uid != want
            {
                continue;
            }
            out.push(c);
        }
        Ok(out)
    }

    /// Return the (contract_uid, confidence) of every Contract a given handler
    /// symbol implements via an `IMPLEMENTS_CONTRACT` edge.
    pub fn contracts_implemented_by(
        &self,
        symbol_uid: &str,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (s:Symbol {uid: $uid})-[r:IMPLEMENTS_CONTRACT]->(c:Contract) \
                 RETURN c.uid, r.confidence";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!("contracts_implemented_by: skipped (table may not exist): {e}");
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("uid", Value::String(symbol_uid.to_string()))],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!("contracts_implemented_by: execute skipped: {e}");
                return Ok(vec![]);
            }
        };
        result
            .map(|row| Ok((extract_string(&row, 0)?, extract_f64(&row, 1)? as f32)))
            .collect()
    }

    /// Return the set of Contract UIDs that have at least one incident
    /// `IMPLEMENTS_CONTRACT` edge (i.e. a handler claims to implement them).
    /// Used by drift diagnostics to compute the declared/implemented diff.
    pub fn list_implemented_contract_uids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (:Symbol)-[:IMPLEMENTS_CONTRACT]->(c:Contract) RETURN DISTINCT c.uid";
        let result = match conn.query(q) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!(
                    "list_implemented_contract_uids: query skipped (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        result.map(|row| extract_string(&row, 0)).collect()
    }

    /// Return implemented Contract UIDs owned by one repository.
    ///
    /// Contract shapes may be identical across repositories. Drift must stay
    /// owner-local even if a damaged or partially migrated database contains
    /// an unexpected edge, so scope both the handler and Contract endpoints.
    pub fn list_implemented_contract_uids_for_repo(
        &self,
        repo_uid: &str,
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = match conn.prepare(
            "MATCH (s:Symbol)-[:IMPLEMENTS_CONTRACT]->(c:Contract) \
             WHERE s.repo_uid = $repo_uid AND c.repo_uid = $repo_uid \
             RETURN DISTINCT c.uid",
        ) {
            Ok(stmt) => stmt,
            Err(error) => {
                tracing::trace!(
                    "list_implemented_contract_uids_for_repo: query skipped \
                     (table may not exist): {error}"
                );
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("repo_uid", Value::String(repo_uid.to_string()))],
        ) {
            Ok(result) => result,
            Err(error) => {
                tracing::trace!(
                    "list_implemented_contract_uids_for_repo: query skipped \
                     (table may not exist): {error}"
                );
                return Ok(vec![]);
            }
        };
        result.map(|row| extract_string(&row, 0)).collect()
    }

    /// Look up a Project by name (case-insensitive).
    pub fn lookup_project_by_name(&self, name: &str) -> Result<Option<Project>, StoreError> {
        let all = self.list_projects()?;
        let needle = name.to_lowercase();
        Ok(all.into_iter().find(|p| p.name.to_lowercase() == needle))
    }

    /// List Note UIDs that belong to a project via PROJECT_INCLUDES_NOTE edges.
    pub fn list_project_note_uids(&self, project_uid: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (p:Project {uid: $uid})-[:PROJECT_INCLUDES_NOTE]->(n:Note) RETURN n.uid";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!("list_project_note_uids: query skipped (table may not exist): {e}");
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("uid", Value::String(project_uid.to_string()))],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!("list_project_note_uids: query skipped (table may not exist): {e}");
                return Ok(vec![]);
            }
        };
        Ok(result
            .filter_map(|row| match row.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    /// List Symbol UIDs that belong to a project via PROJECT_INCLUDES_SYMBOL edges.
    pub fn list_project_symbol_uids(&self, project_uid: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (p:Project {uid: $uid})-[:PROJECT_INCLUDES_SYMBOL]->(s:Symbol) RETURN s.uid";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!(
                    "list_project_symbol_uids: query skipped (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("uid", Value::String(project_uid.to_string()))],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!(
                    "list_project_symbol_uids: query skipped (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        Ok(result
            .filter_map(|row| match row.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    /// Return up to `limit` Symbol UIDs from a project, ranked by PageRank descending.
    ///
    /// Used by `project-context` to seed PPR with the architecturally
    /// important symbols. Seeding them directly is the only way for member
    /// symbols to survive the `min_score` filter — a project that declares
    /// many repos fans out across tens of thousands of
    /// `PROJECT_INCLUDES_SYMBOL` edges, leaving each individual symbol below
    /// threshold when only the project node is seeded.
    pub fn list_project_symbol_uids_by_pagerank(
        &self,
        project_uid: &str,
        limit: usize,
    ) -> Result<Vec<String>, StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Ok(vec![]);
        }
        if limit == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let q = "MATCH (p:Project {uid: $uid})-[:PROJECT_INCLUDES_SYMBOL]->(s:Symbol) \
                 RETURN s.uid, s.pagerank_score \
                 ORDER BY s.pagerank_score DESC \
                 LIMIT $limit";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!(
                    "list_project_symbol_uids_by_pagerank: query skipped \
                     (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![
                ("uid", Value::String(project_uid.to_string())),
                ("limit", Value::Int64(limit as i64)),
            ],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!(
                    "list_project_symbol_uids_by_pagerank: query skipped \
                     (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        Ok(result
            .filter_map(|row| match row.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    /// List component Project UIDs that belong to a project via PROJECT_HAS_COMPONENT edges.
    pub fn list_project_component_uids(
        &self,
        project_uid: &str,
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (p:Project {uid: $uid})-[:PROJECT_HAS_COMPONENT]->(c:Project) RETURN c.uid";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(e) => {
                tracing::trace!(
                    "list_project_component_uids: query skipped (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("uid", Value::String(project_uid.to_string()))],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!(
                    "list_project_component_uids: query skipped (table may not exist): {e}"
                );
                return Ok(vec![]);
            }
        };
        Ok(result
            .filter_map(|row| match row.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    // ── Brain document-graph reads (Feature F9) ─────────────────────────────

    /// Lightweight note rows for orphan detection and topic clustering.
    /// Returns (uid, title, file_path, vault_uid, pagerank_score) per note,
    /// optionally filtered to a single vault. Empty DB → empty vec.
    pub fn list_notes_lite(&self, vault_uid: Option<&str>) -> Result<Vec<NoteLite>, StoreError> {
        let conn = self.conn()?;
        let cols = "n.uid, n.title, n.file_path, n.vault_uid, n.pagerank_score";
        let result = if let Some(vid) = vault_uid {
            let q = format!("MATCH (n:Note) WHERE n.vault_uid = $vid RETURN {cols}");
            let mut stmt = match conn.prepare(&q) {
                Ok(s) => s,
                Err(e) => {
                    tracing::trace!("list_notes_lite: query skipped (table may not exist): {e}");
                    return Ok(vec![]);
                }
            };
            match conn.execute(&mut stmt, vec![("vid", Value::String(vid.to_string()))]) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("list_notes_lite: execute skipped: {e}");
                    return Ok(vec![]);
                }
            }
        } else {
            let q = format!("MATCH (n:Note) RETURN {cols}");
            match conn.query(&q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("list_notes_lite: query skipped (table may not exist): {e}");
                    return Ok(vec![]);
                }
            }
        };
        result
            .map(|row| {
                Ok(NoteLite {
                    uid: extract_string(&row, 0)?,
                    title: extract_string(&row, 1)?,
                    file_path: extract_string(&row, 2)?,
                    vault_uid: extract_string(&row, 3)?,
                    pagerank_score: extract_f64(&row, 4)?,
                })
            })
            .collect()
    }

    /// All Note→Note wikilink edges, as (source_note_uid, target_note_uid).
    ///
    /// Traverses source Note → NOTE_HAS_SECTION → Section -[WIKILINK_TO_NOTE]→
    /// target Note, collapsing the section hop so callers see a note-level
    /// adjacency. Self-loops (a note linking to itself) are dropped. Empty DB
    /// or a vault with no wikilinks → empty vec.
    pub fn note_wikilink_edges(&self) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(:Section)-[:WIKILINK_TO_NOTE]->(dst:Note) \
                 RETURN src.uid, dst.uid";
        let result = match conn.query(q) {
            Ok(r) => r,
            Err(e) => {
                tracing::trace!("note_wikilink_edges: query skipped (table may not exist): {e}");
                return Ok(vec![]);
            }
        };
        let mut edges = Vec::new();
        for row in result {
            let src = extract_string(&row, 0)?;
            let dst = extract_string(&row, 1)?;
            if src != dst {
                edges.push((src, dst));
            }
        }
        Ok(edges)
    }

    /// All F11 typed Note→Note relationship edges, as `(source_uid,
    /// target_uid, rel_table_name)` where `rel_table_name` is one of
    /// `SUPERSEDES`, `DEPENDS_ON`, `CAUSED_BY`, `RELATES_TO`. Generic
    /// wikilinks are NOT included. Empty DB / no typed edges → empty vec.
    pub fn typed_note_edges(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let conn = self.conn()?;
        let mut out = Vec::new();
        for rel in ["SUPERSEDES", "DEPENDS_ON", "CAUSED_BY", "RELATES_TO"] {
            let q = format!("MATCH (a:Note)-[:{rel}]->(b:Note) RETURN a.uid, b.uid");
            let result = match conn.query(&q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("typed_note_edges {rel}: query skipped: {e}");
                    continue;
                }
            };
            for row in result {
                let src = extract_string(&row, 0)?;
                let dst = extract_string(&row, 1)?;
                out.push((src, dst, rel.to_string()));
            }
        }
        Ok(out)
    }

    /// Wikilink edges whose resolution is suspect — confidence below 1.0.
    ///
    /// These are ambiguous or low-priority resolutions (the indexer splits
    /// confidence 1/N across ambiguous title matches and assigns < 1.0 to
    /// alias/same-folder matches). Unresolved links are not stored as edges,
    /// so this surfaces the recoverable "broken-ish" links. Each row carries
    /// the source note, the `display` link text, and the current target.
    /// Empty DB → empty vec.
    pub fn broken_wikilinks(&self) -> Result<Vec<BrokenWikilinkRow>, StoreError> {
        let conn = self.conn()?;
        // Preserve insertion order while de-duplicating: an ambiguous `[[Dup]]`
        // emits one WIKILINK_TO_NOTE edge per candidate (all confidence < 1.0),
        // which would otherwise surface as N near-identical rows. Collapse to
        // one row per (source_uid, wikilink_text).
        let mut order: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashMap<(String, String), BrokenWikilinkRow> =
            std::collections::HashMap::new();

        // 1. Low-confidence / ambiguous resolved links.
        let q = "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(:Section)-[r:WIKILINK_TO_NOTE]->(dst:Note) \
                 WHERE r.confidence < 1.0 \
                 RETURN src.uid, src.file_path, src.title, \
                 CASE WHEN r.target = '' THEN r.display ELSE r.target END, \
                 r.confidence, dst.uid";
        match conn.query(q) {
            Ok(result) => {
                for row in result {
                    let source_uid = extract_string(&row, 0)?;
                    let wikilink_text = extract_string(&row, 3)?;
                    let key = (source_uid.clone(), wikilink_text.clone());
                    if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(key.clone())
                    {
                        order.push(key);
                        slot.insert(BrokenWikilinkRow {
                            source_uid,
                            source_path: extract_string(&row, 1)?,
                            source_title: extract_string(&row, 2)?,
                            wikilink_text,
                            confidence: extract_f64(&row, 4)? as f32,
                            current_target_uid: extract_string(&row, 5)?,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::trace!(
                    "broken_wikilinks: WIKILINK query skipped (table may not exist): {e}"
                );
            }
        }

        // 2. Genuinely-unresolved links (no target note → no edge exists).
        //    These are the truly-broken links; confidence 0.0 and empty target.
        let uq = "MATCH (u:UnresolvedWikilink) \
                  RETURN u.source_note_uid, u.source_path, u.source_title, u.wikilink_text";
        match conn.query(uq) {
            Ok(result) => {
                for row in result {
                    let source_uid = extract_string(&row, 0)?;
                    let wikilink_text = extract_string(&row, 3)?;
                    let key = (source_uid.clone(), wikilink_text.clone());
                    if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(key.clone())
                    {
                        order.push(key);
                        slot.insert(BrokenWikilinkRow {
                            source_uid,
                            source_path: extract_string(&row, 1)?,
                            source_title: extract_string(&row, 2)?,
                            wikilink_text,
                            confidence: 0.0,
                            current_target_uid: String::new(),
                        });
                    }
                }
            }
            Err(e) => {
                tracing::trace!(
                    "broken_wikilinks: UnresolvedWikilink query skipped (table may not exist): {e}"
                );
            }
        }

        Ok(order.into_iter().filter_map(|k| seen.remove(&k)).collect())
    }

    /// Set of Note UIDs that have at least one OUTBOUND wikilink (to a note or
    /// a heading). Used by orphan detection. Empty DB → empty set.
    pub fn note_uids_with_outbound_wikilinks(
        &self,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let mut uids = std::collections::HashSet::new();
        for rel in ["WIKILINK_TO_NOTE", "WIKILINK_TO_HEADING"] {
            let q = format!(
                "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(:Section)-[:{rel}]->() RETURN DISTINCT src.uid"
            );
            let result = match conn.query(&q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!(
                        "note_uids_with_outbound_wikilinks: {rel} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            for row in result {
                if let Ok(uid) = extract_string(&row, 0) {
                    uids.insert(uid);
                }
            }
        }
        Ok(uids)
    }

    /// Set of Note UIDs that are the target of at least one INBOUND wikilink.
    /// Used by orphan detection. Empty DB → empty set.
    pub fn note_uids_with_inbound_wikilinks(
        &self,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let mut uids = std::collections::HashSet::new();
        // Direct note targets.
        let q = "MATCH ()-[:WIKILINK_TO_NOTE]->(dst:Note) RETURN DISTINCT dst.uid";
        if let Ok(result) = conn.query(q) {
            for row in result {
                if let Ok(uid) = extract_string(&row, 0) {
                    uids.insert(uid);
                }
            }
        }
        // Heading targets count as inbound links to the heading's parent note.
        let qh = "MATCH ()-[:WIKILINK_TO_HEADING]->(h:Heading) RETURN DISTINCT h.note_uid";
        if let Ok(result) = conn.query(qh) {
            for row in result {
                if let Ok(uid) = extract_string(&row, 0) {
                    uids.insert(uid);
                }
            }
        }
        Ok(uids)
    }

    /// Tag co-occurrence: returns, per note, the set of tag names attached to
    /// it (via NOTE_TAGGED_WITH, plus SECTION_TAGGED_WITH on its sections).
    /// The caller computes co-occurrence counts. Empty DB → empty vec.
    pub fn note_tag_sets(&self) -> Result<Vec<(String, Vec<String>)>, StoreError> {
        let conn = self.conn()?;
        let mut by_note: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();

        // Note-level tags.
        let qn = "MATCH (n:Note)-[:NOTE_TAGGED_WITH]->(t:Tag) RETURN n.uid, t.name";
        if let Ok(result) = conn.query(qn) {
            for row in result {
                let note = extract_string(&row, 0)?;
                let tag = extract_string(&row, 1)?;
                by_note.entry(note).or_default().insert(tag);
            }
        }
        // Section-level tags roll up to the parent note.
        let qs = "MATCH (n:Note)-[:NOTE_HAS_SECTION]->(s:Section)-[:SECTION_TAGGED_WITH]->(t:Tag) \
                  RETURN n.uid, t.name";
        if let Ok(result) = conn.query(qs) {
            for row in result {
                let note = extract_string(&row, 0)?;
                let tag = extract_string(&row, 1)?;
                by_note.entry(note).or_default().insert(tag);
            }
        }

        Ok(by_note
            .into_iter()
            .map(|(note, tags)| {
                let mut v: Vec<String> = tags.into_iter().collect();
                v.sort();
                (note, v)
            })
            .collect())
    }

    // ── DB-level metadata ───────────────────────────────────────────────────

    /// Read the stored embedding metadata (model ID and dimension).
    ///
    /// Returns `Some((model_id, dimension))` when a record has been written
    /// by [`GraphStore::set_embedding_metadata`], or `None` if the Meta table
    /// does not exist yet (old DB) or the `"embedding"` key has never been set.
    pub fn get_embedding_metadata(&self) -> Result<Option<(String, u32)>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (m:Meta {key: $k}) RETURN m.value";
        let mut stmt = match conn.prepare(q) {
            Ok(s) => s,
            Err(_) => {
                // Meta table doesn't exist on older databases — treat as absent.
                return Ok(None);
            }
        };
        let mut result = match conn.execute(
            &mut stmt,
            vec![("k", Value::String("embedding".to_string()))],
        ) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let row = match result.next() {
            Some(r) => r,
            None => return Ok(None),
        };
        let value = extract_string(&row, 0)?;
        if value.is_empty() {
            return Ok(None);
        }
        // Parse the JSON value stored by set_embedding_metadata.
        // Expected format: {"model_id":"<id>","dimension":<n>}
        let parsed: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| StoreError::Query(format!("parse embedding metadata: {e}")))?;
        let model_id = parsed
            .get("model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let dimension = parsed
            .get("dimension")
            .or_else(|| parsed.get("produced_dimension"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if model_id.is_empty() || dimension == 0 {
            return Ok(None);
        }
        Ok(Some((model_id, dimension)))
    }

    /// Read the complete v2 semantic-space contract. Legacy model/dimension
    /// metadata deliberately returns `None`: it cannot prove tokenizer,
    /// revision, pooling, or normalization compatibility.
    pub fn get_embedding_pipeline(
        &self,
    ) -> Result<Option<nestweaver_schema::EmbeddingPipelineV2>, StoreError> {
        let conn = self.conn()?;
        let mut statement = match conn.prepare("MATCH (m:Meta {key: $k}) RETURN m.value") {
            Ok(statement) => statement,
            Err(_) => return Ok(None),
        };
        let mut rows = match conn.execute(
            &mut statement,
            vec![("k", Value::String("embedding".to_string()))],
        ) {
            Ok(rows) => rows,
            Err(_) => return Ok(None),
        };
        let Some(row) = rows.next() else {
            return Ok(None);
        };
        let encoded = extract_string(&row, 0)?;
        let value: serde_json::Value = serde_json::from_str(&encoded)
            .map_err(|error| StoreError::Query(format!("parse embedding metadata: {error}")))?;
        if value.get("schema_version").is_none() {
            return Ok(None);
        }
        let pipeline: nestweaver_schema::EmbeddingPipelineV2 = serde_json::from_value(value)
            .map_err(|error| StoreError::Query(format!("parse embedding pipeline v2: {error}")))?;
        pipeline.validate().map_err(|error| {
            StoreError::Query(format!("validate embedding pipeline v2: {error}"))
        })?;
        Ok(Some(pipeline))
    }

    /// Repos whose last contract derivation failed, sorted by UID.
    ///
    /// `repo_uid` scopes the answer to a single repo (the drift analysis is
    /// optionally repo-filtered); `None` reports every degraded repo in the DB.
    ///
    /// Reads explicit failure and v2 migration-debt markers. An indexed legacy
    /// database without the v2 generation marker is conservatively degraded
    /// for every repository until each scoped derivation has published.
    pub fn contract_derivation_failures(
        &self,
        repo_uid: Option<&str>,
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let indexed_repos = || -> Result<Vec<String>, StoreError> {
            let rows = conn
                .query("MATCH (r:Repo) RETURN r.uid")
                .map_err(|error| StoreError::Query(format!("list contract-debt repos: {error}")))?;
            rows.map(|row| extract_string(&row, 0)).collect()
        };
        // The Meta table holds a handful of singleton rows, so scanning it and
        // filtering the prefix in Rust avoids depending on a string-predicate
        // dialect for a set this small.
        let mut stmt = match conn.prepare("MATCH (m:Meta) RETURN m.key, m.value") {
            Ok(s) => s,
            // An indexed pre-v2 database with no Meta table owes derivation for
            // every repo; fall through to the generation-debt expansion below.
            Err(_) => {
                let mut repos: Vec<String> = indexed_repos()?
                    .into_iter()
                    .filter(|uid| repo_uid.is_none_or(|want| want == uid))
                    .collect();
                repos.sort();
                return Ok(repos);
            }
        };
        let result = match conn.execute(&mut stmt, vec![]) {
            Ok(r) => r,
            Err(_) => {
                let mut repos: Vec<String> = indexed_repos()?
                    .into_iter()
                    .filter(|uid| repo_uid.is_none_or(|want| want == uid))
                    .collect();
                repos.sort();
                return Ok(repos);
            }
        };
        let failure_prefix = crate::write::CONTRACT_DERIVATION_FAILED_PREFIX;
        let debt_prefix = crate::write::CONTRACT_DERIVATION_DEBT_PREFIX;
        let indexed_repos = indexed_repos()?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let mut generation = None;
        let mut out = std::collections::BTreeSet::new();
        for row in result {
            let Ok(key) = extract_string(&row, 0) else {
                continue;
            };
            if key == crate::write::CONTRACT_DERIVATION_GENERATION_KEY {
                generation = extract_string(&row, 1).ok();
                continue;
            }
            let uid = key
                .strip_prefix(failure_prefix)
                .or_else(|| key.strip_prefix(debt_prefix));
            let Some(uid) = uid else { continue };
            // Failure/debt markers may outlive a removed Repo on databases
            // written by older versions. They must not make a nonexistent
            // repository permanently degrade database-wide contract status.
            if !indexed_repos.contains(uid) {
                continue;
            }
            if repo_uid.is_some_and(|want| want != uid) {
                continue;
            }
            out.insert(uid.to_string());
        }
        if generation.as_deref() != Some(crate::write::CONTRACT_DERIVATION_GENERATION) {
            for uid in indexed_repos {
                if repo_uid.is_none_or(|want| want == uid) {
                    out.insert(uid);
                }
            }
        }
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod corruption_canary_tests {
    use super::*;

    #[test]
    fn nul_byte_is_flagged_corrupt() {
        assert!(string_is_corrupt("bad\0name"));
        assert!(string_is_corrupt("\0\0\0\0"));
        // The real observed garbage-prefix pattern contained a NUL.
        assert!(string_is_corrupt("\u{2}S\u{0}\u{19}y_on_empty_is_noop"));
    }

    #[test]
    fn legitimate_strings_are_not_flagged() {
        // Identifiers.
        assert!(!string_is_corrupt("analyze_blast_radius"));
        assert!(!string_is_corrupt("sym:repo:c37ccf01:abc:def:42"));
        assert!(!string_is_corrupt("crates/nestweaver-store/src/read.rs"));
        // Free text that legitimately contains tabs, newlines, CR, and Unicode
        // — must NOT be flagged (the false-positive trap; Google SRE Ch. 26).
        assert!(!string_is_corrupt(
            "# Heading\n\n- a bullet\twith a tab\r\nand an em-dash — plus ✓ and 日本語"
        ));
        assert!(!string_is_corrupt("fn foo(a: i32) -> i32 {\n    a + 1\n}"));
    }

    #[test]
    fn extract_string_errors_on_nul_never_returns_it() {
        let row = vec![
            Value::String("good".into()),
            Value::String("bad\0val".into()),
        ];
        assert_eq!(extract_string(&row, 0).unwrap(), "good");
        match extract_string(&row, 1) {
            Err(StoreError::CorruptValue { column, .. }) => assert_eq!(column, 1),
            other => panic!("expected CorruptValue, got {other:?}"),
        }
    }

    #[test]
    fn extract_opt_string_errors_on_nul() {
        let row = vec![Value::String("x\0y".into())];
        assert!(matches!(
            extract_opt_string(&row, 0),
            Err(StoreError::CorruptValue { .. })
        ));
    }
}

#[cfg(test)]
mod repo_has_content_tests {
    use super::*;
    use nestweaver_schema::{File, Note, NoteKind, Vault};

    fn make_repo(uid: &str, url: &str, sha: &str) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: sha.to_string(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: None,
        }
    }

    fn insert_code_file(store: &GraphStore, repo_uid: &str, path: &str) {
        store
            .insert_file(&File {
                uid: format!("file:{repo_uid}:{path}"),
                path: path.to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "h".to_string(),
            })
            .unwrap();
    }

    fn insert_vault_with_note(store: &GraphStore, vault_uid: &str, name: &str) {
        store
            .upsert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: name.to_string(),
                root_path: format!("/bare/{name}"),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let n_uid = format!("note:{vault_uid}:a");
        store
            .insert_note(&Note {
                uid: n_uid.clone(),
                vault_uid: vault_uid.to_string(),
                file_path: "a.md".to_string(),
                title: "A".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store.insert_vault_note_edge(vault_uid, &n_uid).unwrap();
    }

    #[test]
    fn empty_repo_has_no_content() {
        let store = GraphStore::in_memory().unwrap();
        let repo = make_repo("repo:r", "https://example.com/r", "abc");
        assert!(!store.repo_has_content(&repo).unwrap());
    }

    #[test]
    fn code_repo_with_file_but_zero_symbols_has_content() {
        let store = GraphStore::in_memory().unwrap();
        insert_code_file(&store, "repo:r", "src/empty.js");
        // A repo whose parsed files yielded no symbols is still healthy:
        // File nodes are written for every parsed file.
        assert!(
            store
                .repo_has_content(&make_repo("repo:r", "https://example.com/r", "abc"))
                .unwrap()
        );
        // Per-repo scoping: another repo's files must not satisfy the probe.
        assert!(
            !store
                .repo_has_content(&make_repo("repo:other", "https://example.com/other", "abc"))
                .unwrap()
        );
    }

    #[test]
    fn server_vault_repo_has_content_via_vault_name_match() {
        let store = GraphStore::in_memory().unwrap();
        // Server-mode vault: Repo.url == Vault.name (both the job's repo URL).
        insert_vault_with_note(&store, "vlt:test:1", "https://example.com/notes");
        assert!(
            store
                .repo_has_content(&make_repo("repo:vault", "https://example.com/notes", "abc"))
                .unwrap()
        );
        // The Repo row stores the trimmed URL while Vault.name may keep the
        // trailing slash — both spellings must match.
        insert_vault_with_note(&store, "vlt:test:2", "https://example.com/slash/");
        assert!(
            store
                .repo_has_content(&make_repo(
                    "repo:slash",
                    "https://example.com/slash/",
                    "abc"
                ))
                .unwrap()
        );
        // A different repo url must not match the vault.
        assert!(
            !store
                .repo_has_content(&make_repo("repo:other", "https://example.com/other", "abc"))
                .unwrap()
        );
    }

    #[test]
    fn index_incomplete_requires_sha_and_no_content() {
        let store = GraphStore::in_memory().unwrap();
        // No SHA yet → not "incomplete" (nothing committed to contradict).
        assert!(
            !store
                .repo_index_incomplete(&make_repo("repo:r", "https://example.com/r", ""))
                .unwrap()
        );
        // SHA committed, no content → incomplete.
        assert!(
            store
                .repo_index_incomplete(&make_repo("repo:r", "https://example.com/r", "abc"))
                .unwrap()
        );
        // SHA committed and content landed → complete.
        insert_code_file(&store, "repo:r", "src/a.js");
        assert!(
            !store
                .repo_index_incomplete(&make_repo("repo:r", "https://example.com/r", "abc"))
                .unwrap()
        );
    }
}

use lbug::Value;
use nestweaver_schema::{
    EntryPointKind, Heading, Note, NoteKind, Project, Repo, Section, Service, Symbol, SymbolKind,
    Tag, Vault, Visibility,
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
}

/// Edge data for clustering: (source_uid, target_uid, confidence).
pub type CodeEdge = (String, String, f64);

/// Combined symbols and edges for clustering algorithms.
pub type CodeGraph = (Vec<SymbolBasic>, Vec<CodeEdge>);

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
fn extract_string(row: &[Value], idx: usize) -> Result<String, StoreError> {
    let val = row
        .get(idx)
        .ok_or_else(|| StoreError::Query(format!("column {idx} out of bounds")))?;
    match val {
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
/// uid, name, kind, repo_uid, file_path, start_line, signature, summary, content_hash, pagerank_score, is_entry_point, entry_point_kind
pub(crate) fn row_to_symbol(row: &[Value]) -> Result<Symbol, StoreError> {
    let uid = extract_string(row, 0)?;
    let name = extract_string(row, 1)?;
    let kind_str = extract_string(row, 2)?;
    let kind = parse_symbol_kind(&kind_str);
    let repo_uid = extract_string(row, 3)?;
    let file_path = extract_string(row, 4)?;
    let start_line = u32::try_from(extract_i64(row, 5)?).unwrap_or(0);
    let signature = extract_string(row, 6)?;
    let summary = extract_opt_string(row, 7)?;
    let content_hash = extract_string(row, 8)?;
    let pagerank_score = extract_f64(row, 9)?;
    let iep_str = extract_string(row, 10).unwrap_or_default();
    let is_entry_point = iep_str == "true";
    let epk_str = extract_opt_string(row, 11).unwrap_or(None);
    let entry_point_kind = epk_str.as_deref().and_then(parse_entry_point_kind);

    Ok(Symbol {
        uid,
        name,
        kind,
        repo_uid,
        file_path,
        start_line,
        signature,
        summary,
        content_hash,
        embedding: None,
        pagerank_score: Some(pagerank_score),
        is_entry_point,
        entry_point_kind,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
    })
}

pub(crate) const SYMBOL_COLUMNS: &str = "s.uid, s.name, s.kind, s.repo_uid, s.file_path, s.start_line, \
     s.signature, s.summary, s.content_hash, s.pagerank_score, s.is_entry_point, s.entry_point_kind";

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
        let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, r.instance_id, r.name";
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
        result
            .map(|row| {
                let uid = extract_string(&row, 0)?;
                let url = extract_string(&row, 1)?;
                let indexed_sha = extract_string(&row, 2)?;
                let staleness = u32::try_from(extract_i64(&row, 3)?).unwrap_or(0);
                let instance_id = extract_string(&row, 4)?;
                let name = extract_opt_string(&row, 5).unwrap_or(None);
                Ok(Repo {
                    uid,
                    url,
                    indexed_sha,
                    staleness_commits_behind: staleness,
                    instance_id,
                    name,
                })
            })
            .collect()
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

    /// Count of all Tag nodes.
    pub fn count_tags(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let result = conn
            .query("MATCH (t:Tag) RETURN t.uid")
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(result.count())
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
    /// edge. Returns the source note (the linker), the section it came
    /// from, and the link confidence + display string.
    pub fn wikilink_sources_to_note(&self, note_uid: &str) -> Result<Vec<BacklinkRow>, StoreError> {
        let conn = self.conn()?;
        // Traverse: source Note ← NOTE_HAS_SECTION ← Section -[r:WIKILINK_TO_NOTE]→ target Note.
        let q = "MATCH (src:Note)-[:NOTE_HAS_SECTION]->(s:Section)-[r:WIKILINK_TO_NOTE]->(n:Note {uid: $uid}) \
                 RETURN src.uid, src.title, src.file_path, s.uid, r.confidence, r.display";
        let mut stmt = conn
            .prepare(q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("uid", Value::String(note_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result
            .map(|row| {
                let source_note_uid = extract_string(&row, 0)?;
                let source_note_title = extract_string(&row, 1)?;
                let source_note_path = extract_string(&row, 2)?;
                let source_section_uid = extract_string(&row, 3)?;
                let confidence = extract_f64(&row, 4)? as f32;
                let display = extract_opt_string(&row, 5)?;
                Ok(BacklinkRow {
                    source_note_uid,
                    source_note_title,
                    source_note_path,
                    source_section_uid,
                    confidence,
                    display,
                })
            })
            .collect()
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

    /// Returns all Symbol nodes and code-level edges for clustering.
    pub fn load_code_symbols_and_edges(&self) -> Result<CodeGraph, StoreError> {
        let conn = self.conn()?;

        let q = "MATCH (s:Symbol) RETURN s.uid, s.name, s.file_path, s.kind";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut symbols = Vec::new();
        for row in result {
            let uid = extract_string(&row, 0)?;
            let name = extract_string(&row, 1)?;
            let file_path = extract_string(&row, 2)?;
            let kind = extract_string(&row, 3)?;
            symbols.push(SymbolBasic {
                uid,
                name,
                file_path,
                kind,
            });
        }

        let edge_types = [
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "USES",
            "ACCESSES",
            "MEMBER_OF",
            "INCLUDES_SYM",
        ];
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

    /// Returns all code-level edges with their type label and confidence.
    ///
    /// Each tuple is `(source_uid, target_uid, edge_type, confidence)`.
    /// Used by graph-export functions that need the relationship type.
    pub fn load_typed_edges(&self) -> Result<Vec<(String, String, String, f64)>, StoreError> {
        let conn = self.conn()?;

        let edge_types = [
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "USES",
            "ACCESSES",
            "MEMBER_OF",
            "INCLUDES_SYM",
        ];
        let mut edges: Vec<(String, String, String, f64)> = Vec::new();
        for et in &edge_types {
            let q =
                format!("MATCH (a:Symbol)-[r:{et}]->(b:Symbol) RETURN a.uid, b.uid, r.confidence");
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
                edges.push((src, dst, et.to_string(), confidence));
            }
        }

        Ok(edges)
    }

    /// Returns all Symbol nodes that `uid` calls (outgoing CALLS edges).
    pub fn callees_of(&self, uid: &str) -> Result<Vec<Symbol>, StoreError> {
        let conn = self.conn()?;
        let cols = SYMBOL_COLUMNS.replace("s.", "t.");
        let q = format!("MATCH (s:Symbol {{uid: $uid}})-[:CALLS]->(t:Symbol) RETURN {cols}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let result = conn
            .execute(&mut stmt, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        result.map(|row| row_to_symbol(&row)).collect()
    }

    /// Returns the set of Note UIDs that are tagged with any of the given tag names.
    pub fn list_note_uids_with_tags(
        &self,
        tag_names: &[String],
    ) -> Result<std::collections::HashSet<String>, StoreError> {
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

    /// Returns the set of Section UIDs that are tagged with any of the given tag names.
    pub fn list_section_uids_with_tags(
        &self,
        tag_names: &[String],
    ) -> Result<std::collections::HashSet<String>, StoreError> {
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
        let result =
            match conn.execute(&mut stmt, vec![("since", Value::String(since.to_string()))]) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("list_note_uids_modified_since: execute error: {e}");
                    return Ok(std::collections::HashSet::new());
                }
            };
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
            let result =
                match conn.execute(&mut stmt, vec![("nid", Value::String(note_uid.clone()))]) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::trace!("list_section_uids_modified_since: execute error: {e}");
                        continue;
                    }
                };
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
}

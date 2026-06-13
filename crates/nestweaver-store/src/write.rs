use nestweaver_schema::{
    Contract, EdgeType, File, Heading, Note, Project, Repo, ResolvedEdge, Section, Service, Symbol,
    Tag, Vault,
    uid::{project_uid, repo_uid, vault_uid},
};
use serde_json;

use crate::db::GraphStore;
use crate::error::StoreError;

/// A vault whose notes were removed during an instance merge.
#[derive(Debug)]
pub struct UnlinkedVault {
    pub root_path: String,
    pub notes_removed: usize,
}

/// Result of [`GraphStore::merge_instance_ids`].
#[derive(Debug)]
pub struct MergeResult {
    pub vaults: usize,
    pub repos: usize,
    pub projects: usize,
    pub unlinked: Vec<UnlinkedVault>,
}

/// Result of [`GraphStore::reparent_vault`].
#[derive(Debug)]
pub struct ReparentVaultResult {
    pub notes_migrated: usize,
    pub headings_migrated: usize,
    pub sections_migrated: usize,
    pub tags_migrated: usize,
}

/// Result of [`GraphStore::purge_instance`]. Reports how many top-level
/// rows were cascade-deleted from the graph for the given instance,
/// plus a separate count for orphan nodes (Symbol/File/Service/Note/
/// Heading/Section/Tag rows whose UID prefix encodes the instance but
/// whose parent Repo or Vault no longer exists — typically left behind
/// by a partially-applied `instance merge`).
#[derive(Debug, Default)]
pub struct PurgeInstanceResult {
    pub repos: usize,
    pub files: usize,
    pub symbols: usize,
    pub vaults: usize,
    pub notes: usize,
    pub projects: usize,
    pub orphans_swept: usize,
}

/// Encode a Symbol's `framework_hint` as the `"framework:role"` string the
/// `framework_hint` column stores. Returns an empty string when absent.
fn encode_framework_hint(symbol: &Symbol) -> String {
    match &symbol.framework_hint {
        Some(h) => format!("{}:{}", h.framework, h.role),
        None => String::new(),
    }
}

fn exec_params(
    conn: &lbug::Connection<'_>,
    query: &str,
    params: Vec<(&str, lbug::Value)>,
) -> Result<(), StoreError> {
    let mut stmt = conn
        .prepare(query)
        .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
    conn.execute(&mut stmt, params)
        .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
    Ok(())
}

impl GraphStore {
    pub fn insert_repo(&self, repo: &Repo) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
             staleness_commits_behind: $scb, instance_id: $iid, name: $name})",
            vec![
                ("uid", lbug::Value::String(repo.uid.clone())),
                ("url", lbug::Value::String(repo.url.clone())),
                ("sha", lbug::Value::String(repo.indexed_sha.clone())),
                (
                    "scb",
                    lbug::Value::Int64(repo.staleness_commits_behind as i64),
                ),
                ("iid", lbug::Value::String(repo.instance_id.clone())),
                (
                    "name",
                    lbug::Value::String(repo.name.clone().unwrap_or_default()),
                ),
            ],
        )
    }

    pub fn insert_file(&self, file: &File) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:File {uid: $uid, path: $path, repo_uid: $repo, content_hash: $hash})",
            vec![
                ("uid", lbug::Value::String(file.uid.clone())),
                ("path", lbug::Value::String(file.path.clone())),
                ("repo", lbug::Value::String(file.repo_uid.clone())),
                ("hash", lbug::Value::String(file.content_hash.clone())),
            ],
        )
    }

    pub fn insert_service(&self, service: &Service) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Service {uid: $uid, name: $name, repo_uid: $repo, \
             summary: $summary, summary_hash: $shash})",
            vec![
                ("uid", lbug::Value::String(service.uid.clone())),
                ("name", lbug::Value::String(service.name.clone())),
                ("repo", lbug::Value::String(service.repo_uid.clone())),
                (
                    "summary",
                    lbug::Value::String(service.summary.clone().unwrap_or_default()),
                ),
                (
                    "shash",
                    lbug::Value::String(service.summary_hash.clone().unwrap_or_default()),
                ),
            ],
        )
    }

    pub fn insert_symbol(&self, symbol: &Symbol) -> Result<(), StoreError> {
        let conn = self.conn()?;
        self.insert_symbol_with_conn(&conn, symbol)
    }

    pub(crate) fn insert_symbol_with_conn(
        &self,
        conn: &lbug::Connection<'_>,
        symbol: &Symbol,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "CREATE (:Symbol {uid: $uid, name: $name, kind: $kind, \
             repo_uid: $repo, file_path: $fp, start_line: $sl, end_line: $el, \
             signature: $sig, summary: $summary, content_hash: $hash, \
             pagerank_score: $pr, is_entry_point: $iep, entry_point_kind: $epk, \
             framework_hint: $fh})",
            vec![
                ("uid", lbug::Value::String(symbol.uid.clone())),
                ("name", lbug::Value::String(symbol.name.clone())),
                ("kind", lbug::Value::String(symbol.kind.to_string())),
                ("repo", lbug::Value::String(symbol.repo_uid.clone())),
                ("fp", lbug::Value::String(symbol.file_path.clone())),
                ("sl", lbug::Value::Int64(symbol.start_line as i64)),
                ("el", lbug::Value::Int64(symbol.end_line as i64)),
                ("sig", lbug::Value::String(symbol.signature.clone())),
                (
                    "summary",
                    lbug::Value::String(symbol.summary.clone().unwrap_or_default()),
                ),
                ("hash", lbug::Value::String(symbol.content_hash.clone())),
                (
                    "pr",
                    lbug::Value::Double(symbol.pagerank_score.unwrap_or(0.0)),
                ),
                (
                    "iep",
                    lbug::Value::String(
                        if symbol.is_entry_point {
                            "true"
                        } else {
                            "false"
                        }
                        .to_string(),
                    ),
                ),
                (
                    "epk",
                    lbug::Value::String(
                        symbol
                            .entry_point_kind
                            .map(|k| k.to_string())
                            .unwrap_or_default(),
                    ),
                ),
                ("fh", lbug::Value::String(encode_framework_hint(symbol))),
            ],
        )
    }

    pub fn batch_insert_symbols(&self, symbols: &[Symbol]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_symbols_on(&conn, symbols)
    }

    /// Insert symbols using an externally-provided connection (for transaction batching).
    pub fn batch_insert_symbols_on(
        conn: &lbug::Connection<'_>,
        symbols: &[Symbol],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Symbol {uid: $uid, name: $name, kind: $kind, \
                 repo_uid: $repo, file_path: $fp, start_line: $sl, end_line: $el, \
                 signature: $sig, summary: $summary, content_hash: $hash, \
                 pagerank_score: $pr, is_entry_point: $iep, entry_point_kind: $epk, \
                 framework_hint: $fh})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for symbol in symbols {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(symbol.uid.clone())),
                    ("name", lbug::Value::String(symbol.name.clone())),
                    ("kind", lbug::Value::String(symbol.kind.to_string())),
                    ("repo", lbug::Value::String(symbol.repo_uid.clone())),
                    ("fp", lbug::Value::String(symbol.file_path.clone())),
                    ("sl", lbug::Value::Int64(symbol.start_line as i64)),
                    ("el", lbug::Value::Int64(symbol.end_line as i64)),
                    ("sig", lbug::Value::String(symbol.signature.clone())),
                    (
                        "summary",
                        lbug::Value::String(symbol.summary.clone().unwrap_or_default()),
                    ),
                    ("hash", lbug::Value::String(symbol.content_hash.clone())),
                    (
                        "pr",
                        lbug::Value::Double(symbol.pagerank_score.unwrap_or(0.0)),
                    ),
                    (
                        "iep",
                        lbug::Value::String(
                            if symbol.is_entry_point {
                                "true"
                            } else {
                                "false"
                            }
                            .to_string(),
                        ),
                    ),
                    (
                        "epk",
                        lbug::Value::String(
                            symbol
                                .entry_point_kind
                                .map(|k| k.to_string())
                                .unwrap_or_default(),
                        ),
                    ),
                    ("fh", lbug::Value::String(encode_framework_hint(symbol))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_files(&self, files: &[File]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_files_on(&conn, files)
    }

    /// Insert files using an externally-provided connection (for transaction batching).
    pub fn batch_insert_files_on(
        conn: &lbug::Connection<'_>,
        files: &[File],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:File {uid: $uid, path: $path, repo_uid: $repo, content_hash: $hash})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for file in files {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(file.uid.clone())),
                    ("path", lbug::Value::String(file.path.clone())),
                    ("repo", lbug::Value::String(file.repo_uid.clone())),
                    ("hash", lbug::Value::String(file.content_hash.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_repo_file_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_repo_file_edges_on(&conn, edges)
    }

    /// Insert repo-file edges using an externally-provided connection.
    pub fn batch_insert_repo_file_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (r:Repo {uid: $repo}), (f:File {uid: $file}) \
                 CREATE (r)-[:REPO_HAS_FILE]->(f)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (repo_uid, file_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("repo", lbug::Value::String(repo_uid.to_string())),
                    ("file", lbug::Value::String(file_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_file_symbol_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_file_symbol_edges_on(&conn, edges)
    }

    /// Insert file-symbol edges using an externally-provided connection.
    pub fn batch_insert_file_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (f:File {uid: $file}), (s:Symbol {uid: $sym}) \
                 CREATE (f)-[:FILE_HAS_SYMBOL]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (file_uid, symbol_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("file", lbug::Value::String(file_uid.to_string())),
                    ("sym", lbug::Value::String(symbol_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_repo_file_edge(&self, repo_uid: &str, file_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (r:Repo {uid: $repo}), (f:File {uid: $file}) \
             CREATE (r)-[:REPO_HAS_FILE]->(f)",
            vec![
                ("repo", lbug::Value::String(repo_uid.to_string())),
                ("file", lbug::Value::String(file_uid.to_string())),
            ],
        )
    }

    pub fn insert_file_symbol_edge(
        &self,
        file_uid: &str,
        symbol_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (f:File {uid: $file}), (s:Symbol {uid: $sym}) \
             CREATE (f)-[:FILE_HAS_SYMBOL]->(s)",
            vec![
                ("file", lbug::Value::String(file_uid.to_string())),
                ("sym", lbug::Value::String(symbol_uid.to_string())),
            ],
        )
    }

    pub fn insert_service_symbol_edge(
        &self,
        service_uid: &str,
        symbol_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (svc:Service {uid: $svc}), (sym:Symbol {uid: $sym}) \
             CREATE (svc)-[:SERVICE_HAS_SYMBOL]->(sym)",
            vec![
                ("svc", lbug::Value::String(service_uid.to_string())),
                ("sym", lbug::Value::String(symbol_uid.to_string())),
            ],
        )
    }

    pub fn batch_insert_service_symbol_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_service_symbol_edges_on(&conn, edges)
    }

    /// Insert service-symbol edges using an externally-provided connection.
    pub fn batch_insert_service_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (svc:Service {uid: $svc}), (sym:Symbol {uid: $sym}) \
                 CREATE (svc)-[:SERVICE_HAS_SYMBOL]->(sym)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (service_uid, symbol_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("svc", lbug::Value::String(service_uid.to_string())),
                    ("sym", lbug::Value::String(symbol_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_edge(&self, edge: &ResolvedEdge) -> Result<(), StoreError> {
        let conn = self.conn()?;
        self.insert_edge_with_conn(&conn, edge)
    }

    pub(crate) fn insert_edge_with_conn(
        &self,
        conn: &lbug::Connection<'_>,
        edge: &ResolvedEdge,
    ) -> Result<(), StoreError> {
        let src = edge.source_uid.clone();
        let tgt = edge.target_uid.clone();
        let conf = edge.confidence as f64;
        let evidence_json = if edge.evidence.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&edge.evidence).unwrap_or_default()
        };

        match edge.edge_type {
            EdgeType::Calls => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:CALLS {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Imports => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:IMPORTS {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Extends => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:EXTENDS_SYM {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Implements => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:IMPLEMENTS_SYM {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Includes => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:INCLUDES_SYM {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Uses => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:USES {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Accesses => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:ACCESSES {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::MemberOf => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:MEMBER_OF {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Contains => Err(StoreError::Query(
                "Use insert_repo_file_edge / insert_file_symbol_edge for CONTAINS edges"
                    .to_string(),
            )),
            EdgeType::CrossRepoLink => {
                let link_type = edge
                    .link_type
                    .map(|lt| format!("{lt:?}"))
                    .unwrap_or_default();
                exec_params(
                    conn,
                    "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                     CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt, evidence: $ev}]->(b)",
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ],
                )
            }
            EdgeType::ImplementsContract => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Contract {uid: $tgt}) \
                 CREATE (a)-[:IMPLEMENTS_CONTRACT {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Supersedes
            | EdgeType::DependsOn
            | EdgeType::CausedBy
            | EdgeType::RelatesTo => {
                // F11 typed Note→Note relationships.
                let rel = edge.edge_type.rel_table_name();
                let q = format!(
                    "MATCH (a:Note {{uid: $src}}), (b:Note {{uid: $tgt}}) \
                     CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                );
                exec_params(
                    conn,
                    &q,
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ],
                )
            }
            EdgeType::ProjectIncludesSymbol
            | EdgeType::ProjectIncludesNote
            | EdgeType::ProjectHasComponent
            | EdgeType::ProjectHasParent => Err(StoreError::Query(
                "Use batch_insert_project_symbol_edges / batch_insert_project_note_edges / \
                 insert_project_component_edge / insert_project_parent_edge for Project edges"
                    .to_string(),
            )),
        }
    }

    /// Perform all bulk inserts for a full index in a single transaction.
    /// This avoids per-statement WAL flushes and provides a major speedup.
    pub fn bulk_index_write(
        &self,
        files: &[File],
        symbols: &[Symbol],
        repo_file_edges: &[(&str, &str)],
        file_symbol_edges: &[(&str, &str)],
        services: &[Service],
        service_symbol_edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;

        // Insert file nodes.
        Self::batch_insert_files_on(&conn, files)?;

        // Insert symbol nodes.
        Self::batch_insert_symbols_on(&conn, symbols)?;

        // Insert REPO_HAS_FILE edges.
        Self::batch_insert_repo_file_edges_on(&conn, repo_file_edges)?;

        // Insert FILE_HAS_SYMBOL edges.
        Self::batch_insert_file_symbol_edges_on(&conn, file_symbol_edges)?;

        // Insert service nodes.
        {
            let mut stmt = conn
                .prepare(
                    "CREATE (:Service {uid: $uid, name: $name, repo_uid: $repo, \
                     summary: $summary, summary_hash: $shash})",
                )
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            for svc in services {
                conn.execute(
                    &mut stmt,
                    vec![
                        ("uid", lbug::Value::String(svc.uid.clone())),
                        ("name", lbug::Value::String(svc.name.clone())),
                        ("repo", lbug::Value::String(svc.repo_uid.clone())),
                        (
                            "summary",
                            lbug::Value::String(svc.summary.clone().unwrap_or_default()),
                        ),
                        (
                            "shash",
                            lbug::Value::String(svc.summary_hash.clone().unwrap_or_default()),
                        ),
                    ],
                )
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            }
        }

        // Insert SERVICE_HAS_SYMBOL edges.
        Self::batch_insert_service_symbol_edges_on(&conn, service_symbol_edges)?;

        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Wrap all markdown vault inserts in a single transaction.
    ///
    /// Accepts the full set of data produced by `index_into_store` (notes,
    /// headings, sections, structural edges, tags, and cross-reference edges)
    /// and writes everything atomically, avoiding per-statement WAL flushes.
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_vault_write(
        &self,
        notes: &[Note],
        headings: &[Heading],
        sections: &[Section],
        vault_note_edges: &[(&str, &str)],
        note_heading_edges: &[(&str, &str)],
        note_section_edges: &[(&str, &str)],
        heading_section_edges: &[(&str, &str)],
        heading_parent_edges: &[(&str, &str)],
        tags: &[Tag],
        note_tag_edges: &[(&str, &str)],
        section_tag_edges: &[(&str, &str)],
        wikilink_to_note_edges: &[(&str, &str, f32, &str)],
        wikilink_to_heading_edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;

        // Insert node tables first so edge MATCH clauses find their endpoints.
        Self::batch_insert_notes_on(&conn, notes)?;
        Self::batch_insert_headings_on(&conn, headings)?;
        Self::batch_insert_sections_on(&conn, sections)?;

        // Structural containment edges.
        Self::batch_insert_vault_note_edges_on(&conn, vault_note_edges)?;
        Self::batch_insert_note_heading_edges_on(&conn, note_heading_edges)?;
        Self::batch_insert_note_section_edges_on(&conn, note_section_edges)?;
        Self::batch_insert_heading_section_edges_on(&conn, heading_section_edges)?;
        Self::batch_insert_heading_parent_edges_on(&conn, heading_parent_edges)?;

        // Tags (nodes + edges). Tags may already exist from a previous index
        // run; the caller is responsible for deduplicating `tags` by uid before
        // passing them in.
        Self::batch_insert_tags_on(&conn, tags)?;
        Self::batch_insert_note_tag_edges_on(&conn, note_tag_edges)?;
        Self::batch_insert_section_tag_edges_on(&conn, section_tag_edges)?;

        // Cross-reference wikilink edges.
        Self::batch_insert_wikilink_to_note_edges_on(&conn, wikilink_to_note_edges)?;
        Self::batch_insert_wikilink_to_heading_edges_on(&conn, wikilink_to_heading_edges)?;

        self.commit_transaction(&conn)?;
        Ok(())
    }

    pub fn batch_insert_edges(&self, edges: &[ResolvedEdge]) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;
        Self::batch_insert_edges_on(&conn, edges)?;
        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Insert resolved edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[ResolvedEdge],
    ) -> Result<(), StoreError> {
        // Group edges by their SQL query string so we prepare each statement only once.
        use std::collections::HashMap;

        // Collect (query_string, params) pairs grouped by query.
        let mut groups: HashMap<String, Vec<Vec<(&str, lbug::Value)>>> = HashMap::new();

        for edge in edges {
            let src = edge.source_uid.clone();
            let tgt = edge.target_uid.clone();
            let conf = edge.confidence as f64;
            let evidence_json = if edge.evidence.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&edge.evidence).unwrap_or_default()
            };

            match edge.edge_type {
                EdgeType::Calls => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:CALLS {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Imports => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:IMPORTS {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Extends => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:EXTENDS_SYM {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Implements => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:IMPLEMENTS_SYM {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Includes => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:INCLUDES_SYM {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Uses => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:USES {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Accesses => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:ACCESSES {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::MemberOf => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:MEMBER_OF {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Contains => {
                    return Err(StoreError::Query(
                        "Use insert_repo_file_edge / insert_file_symbol_edge for CONTAINS edges"
                            .to_string(),
                    ));
                }
                EdgeType::CrossRepoLink => {
                    let link_type = edge
                        .link_type
                        .map(|lt| format!("{lt:?}"))
                        .unwrap_or_default();
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::ImplementsContract => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Contract {uid: $tgt}) \
                               CREATE (a)-[:IMPLEMENTS_CONTRACT {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Supersedes
                | EdgeType::DependsOn
                | EdgeType::CausedBy
                | EdgeType::RelatesTo => {
                    // F11 typed Note→Note relationships.
                    let rel = edge.edge_type.rel_table_name();
                    let key = format!(
                        "MATCH (a:Note {{uid: $src}}), (b:Note {{uid: $tgt}}) \
                         CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                    );
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::ProjectIncludesSymbol
                | EdgeType::ProjectIncludesNote
                | EdgeType::ProjectHasComponent
                | EdgeType::ProjectHasParent => {
                    return Err(StoreError::Query(
                        "Use batch_insert_project_symbol_edges / batch_insert_project_note_edges / \
                         insert_project_component_edge / insert_project_parent_edge for Project edges"
                            .to_string(),
                    ));
                }
            }
        }

        for (query, param_sets) in &groups {
            let mut stmt = conn
                .prepare(query)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            for params in param_sets {
                conn.execute(&mut stmt, params.clone())
                    .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            }
        }
        Ok(())
    }

    // ── Brain extension: markdown node inserts ──────────────────────────────

    pub fn insert_vault(&self, vault: &Vault) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Vault {uid: $uid, name: $name, root_path: $rp, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(vault.uid.clone())),
                ("name", lbug::Value::String(vault.name.clone())),
                ("rp", lbug::Value::String(vault.root_path.clone())),
                ("iid", lbug::Value::String(vault.instance_id.clone())),
            ],
        )
    }

    pub fn upsert_vault(&self, vault: &Vault) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let _ = exec_params(
            &conn,
            "MATCH (v:Vault {uid: $uid}) DETACH DELETE v",
            vec![("uid", lbug::Value::String(vault.uid.clone()))],
        );
        self.insert_vault(vault)
    }

    pub fn insert_note(&self, note: &Note) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Note {uid: $uid, vault_uid: $vid, file_path: $fp, title: $title, \
             note_kind: $nk, word_count: $wc, content_hash: $hash, frontmatter: $fm, \
             created_at: $ca, modified_at: $ma, pagerank_score: $pr})",
            vec![
                ("uid", lbug::Value::String(note.uid.clone())),
                ("vid", lbug::Value::String(note.vault_uid.clone())),
                ("fp", lbug::Value::String(note.file_path.clone())),
                ("title", lbug::Value::String(note.title.clone())),
                ("nk", lbug::Value::String(note.note_kind.to_string())),
                ("wc", lbug::Value::Int64(note.word_count as i64)),
                ("hash", lbug::Value::String(note.content_hash.clone())),
                (
                    "fm",
                    lbug::Value::String(note.frontmatter.clone().unwrap_or_default()),
                ),
                (
                    "ca",
                    lbug::Value::String(note.created_at.clone().unwrap_or_default()),
                ),
                (
                    "ma",
                    lbug::Value::String(note.modified_at.clone().unwrap_or_default()),
                ),
                (
                    "pr",
                    lbug::Value::Double(note.pagerank_score.unwrap_or(0.0)),
                ),
            ],
        )
    }

    pub fn batch_insert_notes(&self, notes: &[Note]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_notes_on(&conn, notes)
    }

    /// Insert notes using an externally-provided connection (for transaction batching).
    pub fn batch_insert_notes_on(
        conn: &lbug::Connection<'_>,
        notes: &[Note],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Note {uid: $uid, vault_uid: $vid, file_path: $fp, title: $title, \
                 note_kind: $nk, word_count: $wc, content_hash: $hash, frontmatter: $fm, \
                 created_at: $ca, modified_at: $ma, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for note in notes {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(note.uid.clone())),
                    ("vid", lbug::Value::String(note.vault_uid.clone())),
                    ("fp", lbug::Value::String(note.file_path.clone())),
                    ("title", lbug::Value::String(note.title.clone())),
                    ("nk", lbug::Value::String(note.note_kind.to_string())),
                    ("wc", lbug::Value::Int64(note.word_count as i64)),
                    ("hash", lbug::Value::String(note.content_hash.clone())),
                    (
                        "fm",
                        lbug::Value::String(note.frontmatter.clone().unwrap_or_default()),
                    ),
                    (
                        "ca",
                        lbug::Value::String(note.created_at.clone().unwrap_or_default()),
                    ),
                    (
                        "ma",
                        lbug::Value::String(note.modified_at.clone().unwrap_or_default()),
                    ),
                    (
                        "pr",
                        lbug::Value::Double(note.pagerank_score.unwrap_or(0.0)),
                    ),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_vault_note_edge(
        &self,
        vault_uid: &str,
        note_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (v:Vault {uid: $vid}), (n:Note {uid: $nid}) \
             CREATE (v)-[:VAULT_HAS_NOTE]->(n)",
            vec![
                ("vid", lbug::Value::String(vault_uid.to_string())),
                ("nid", lbug::Value::String(note_uid.to_string())),
            ],
        )
    }

    pub fn batch_insert_vault_note_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_vault_note_edges_on(&conn, edges)
    }

    /// Insert vault-note edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_vault_note_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (v:Vault {uid: $vid}), (n:Note {uid: $nid}) \
                 CREATE (v)-[:VAULT_HAS_NOTE]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (vault_uid, note_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("vid", lbug::Value::String(vault_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Brain extension: Heading and Section inserts ────────────────────────

    pub fn insert_heading(&self, heading: &Heading) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Heading {uid: $uid, note_uid: $nid, level: $lvl, text: $text, \
             slug: $slug, start_line: $sl, end_line: $el, content_hash: $hash})",
            vec![
                ("uid", lbug::Value::String(heading.uid.clone())),
                ("nid", lbug::Value::String(heading.note_uid.clone())),
                ("lvl", lbug::Value::Int64(heading.level as i64)),
                ("text", lbug::Value::String(heading.text.clone())),
                ("slug", lbug::Value::String(heading.slug.clone())),
                ("sl", lbug::Value::Int64(heading.start_line as i64)),
                ("el", lbug::Value::Int64(heading.end_line as i64)),
                ("hash", lbug::Value::String(heading.content_hash.clone())),
            ],
        )
    }

    pub fn batch_insert_headings(&self, headings: &[Heading]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_headings_on(&conn, headings)
    }

    /// Insert headings using an externally-provided connection (for transaction batching).
    pub fn batch_insert_headings_on(
        conn: &lbug::Connection<'_>,
        headings: &[Heading],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Heading {uid: $uid, note_uid: $nid, level: $lvl, text: $text, \
                 slug: $slug, start_line: $sl, end_line: $el, content_hash: $hash})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for h in headings {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(h.uid.clone())),
                    ("nid", lbug::Value::String(h.note_uid.clone())),
                    ("lvl", lbug::Value::Int64(h.level as i64)),
                    ("text", lbug::Value::String(h.text.clone())),
                    ("slug", lbug::Value::String(h.slug.clone())),
                    ("sl", lbug::Value::Int64(h.start_line as i64)),
                    ("el", lbug::Value::Int64(h.end_line as i64)),
                    ("hash", lbug::Value::String(h.content_hash.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_section(&self, section: &Section) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
             start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
             word_count: $wc, pagerank_score: $pr})",
            vec![
                ("uid", lbug::Value::String(section.uid.clone())),
                ("nid", lbug::Value::String(section.note_uid.clone())),
                (
                    "hid",
                    lbug::Value::String(section.heading_uid.clone().unwrap_or_default()),
                ),
                ("sl", lbug::Value::Int64(section.start_line as i64)),
                ("el", lbug::Value::Int64(section.end_line as i64)),
                ("th", lbug::Value::String(section.text_hash.clone())),
                ("tc", lbug::Value::String(section.text_content.clone())),
                ("wc", lbug::Value::Int64(section.word_count as i64)),
                (
                    "pr",
                    lbug::Value::Double(section.pagerank_score.unwrap_or(0.0)),
                ),
            ],
        )
    }

    pub fn batch_insert_sections(&self, sections: &[Section]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_sections_on(&conn, sections)
    }

    /// Insert sections using an externally-provided connection (for transaction batching).
    pub fn batch_insert_sections_on(
        conn: &lbug::Connection<'_>,
        sections: &[Section],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
                 start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
                 word_count: $wc, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for s in sections {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(s.uid.clone())),
                    ("nid", lbug::Value::String(s.note_uid.clone())),
                    (
                        "hid",
                        lbug::Value::String(s.heading_uid.clone().unwrap_or_default()),
                    ),
                    ("sl", lbug::Value::Int64(s.start_line as i64)),
                    ("el", lbug::Value::Int64(s.end_line as i64)),
                    ("th", lbug::Value::String(s.text_hash.clone())),
                    ("tc", lbug::Value::String(s.text_content.clone())),
                    ("wc", lbug::Value::Int64(s.word_count as i64)),
                    ("pr", lbug::Value::Double(s.pagerank_score.unwrap_or(0.0))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_heading_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_heading_edges_on(&conn, edges)
    }

    /// Insert note-heading edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_heading_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (h:Heading {uid: $hid}) \
                 CREATE (n)-[:NOTE_HAS_HEADING]->(h)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, heading_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("hid", lbug::Value::String(heading_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_section_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_section_edges_on(&conn, edges)
    }

    /// Insert note-section edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_section_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (s:Section {uid: $sid}) \
                 CREATE (n)-[:NOTE_HAS_SECTION]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, section_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("sid", lbug::Value::String(section_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_heading_section_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_heading_section_edges_on(&conn, edges)
    }

    /// Insert heading-section edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_heading_section_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (h:Heading {uid: $hid}), (s:Section {uid: $sid}) \
                 CREATE (h)-[:HEADING_HAS_SECTION]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (heading_uid, section_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("hid", lbug::Value::String(heading_uid.to_string())),
                    ("sid", lbug::Value::String(section_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_heading_parent_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_heading_parent_edges_on(&conn, edges)
    }

    /// Insert heading-parent edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_heading_parent_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (child:Heading {uid: $cid}), (parent:Heading {uid: $pid}) \
                 CREATE (child)-[:HEADING_PARENT]->(parent)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (child_uid, parent_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("cid", lbug::Value::String(child_uid.to_string())),
                    ("pid", lbug::Value::String(parent_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Brain extension: Tag + Project + cross-reference edges ──────────────

    pub fn insert_tag(&self, tag: &Tag) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Tag {uid: $uid, vault_uid: $vid, name: $name})",
            vec![
                ("uid", lbug::Value::String(tag.uid.clone())),
                ("vid", lbug::Value::String(tag.vault_uid.clone())),
                ("name", lbug::Value::String(tag.name.clone())),
            ],
        )
    }

    pub fn batch_insert_tags(&self, tags: &[Tag]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_tags_on(&conn, tags)
    }

    /// Insert tags using an externally-provided connection (for transaction batching).
    pub fn batch_insert_tags_on(
        conn: &lbug::Connection<'_>,
        tags: &[Tag],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare("CREATE (:Tag {uid: $uid, vault_uid: $vid, name: $name})")
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for t in tags {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(t.uid.clone())),
                    ("vid", lbug::Value::String(t.vault_uid.clone())),
                    ("name", lbug::Value::String(t.name.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_project(&self, project: &Project) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Project {uid: $uid, name: $name, summary: $summary, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(project.uid.clone())),
                ("name", lbug::Value::String(project.name.clone())),
                (
                    "summary",
                    lbug::Value::String(project.summary.clone().unwrap_or_default()),
                ),
                ("iid", lbug::Value::String(project.instance_id.clone())),
            ],
        )
    }

    /// Record a genuinely-unresolved wikilink (`[[Target]]` with no matching
    /// note) so the broken-links query can surface it. `uid` is derived from
    /// the source section + link text by the caller so re-indexing the same
    /// note replaces rather than duplicates. DETACH DELETE-by-uid first makes
    /// the insert idempotent. Table may not exist on older DBs — caller treats
    /// errors as best-effort.
    pub fn insert_unresolved_wikilink(
        &self,
        uid: &str,
        source_note_uid: &str,
        source_path: &str,
        source_title: &str,
        wikilink_text: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (u:UnresolvedWikilink {uid: $uid}) DETACH DELETE u",
            vec![("uid", lbug::Value::String(uid.to_string()))],
        )?;
        exec_params(
            &conn,
            "CREATE (:UnresolvedWikilink {uid: $uid, source_note_uid: $snu, \
             source_path: $sp, source_title: $st, wikilink_text: $wt})",
            vec![
                ("uid", lbug::Value::String(uid.to_string())),
                ("snu", lbug::Value::String(source_note_uid.to_string())),
                ("sp", lbug::Value::String(source_path.to_string())),
                ("st", lbug::Value::String(source_title.to_string())),
                ("wt", lbug::Value::String(wikilink_text.to_string())),
            ],
        )
    }

    /// Remove all recorded unresolved wikilinks originating from `note_uid`.
    /// Called from `delete_note_cascade` so stale rows do not linger after a
    /// note is re-indexed (e.g. once its target note appears). Best-effort:
    /// silently succeeds if the table does not exist.
    pub fn delete_unresolved_wikilinks_for_note(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        if let Err(e) = exec_params(
            &conn,
            "MATCH (u:UnresolvedWikilink {source_note_uid: $uid}) DETACH DELETE u",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        ) {
            tracing::trace!("delete_unresolved_wikilinks_for_note skipped: {e}");
        }
        Ok(())
    }

    /// Insert (or idempotently replace) a Contract node. Mirrors
    /// `insert_project`: DETACH DELETE by UID first so re-indexing a spec
    /// or handler does not accumulate duplicate Contract nodes.
    pub fn insert_contract(&self, contract: &Contract) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (c:Contract {uid: $uid}) DETACH DELETE c",
            vec![("uid", lbug::Value::String(contract.uid.clone()))],
        )?;
        exec_params(
            &conn,
            "CREATE (:Contract {uid: $uid, kind: $kind, verb: $verb, path: $path, \
             operation_id: $op, repo_uid: $repo, source_path: $src, confidence: $conf})",
            vec![
                ("uid", lbug::Value::String(contract.uid.clone())),
                ("kind", lbug::Value::String(contract.kind.clone())),
                (
                    "verb",
                    lbug::Value::String(contract.verb.clone().unwrap_or_default()),
                ),
                (
                    "path",
                    lbug::Value::String(contract.path.clone().unwrap_or_default()),
                ),
                (
                    "op",
                    lbug::Value::String(contract.operation_id.clone().unwrap_or_default()),
                ),
                ("repo", lbug::Value::String(contract.repo_uid.clone())),
                ("src", lbug::Value::String(contract.source_path.clone())),
                ("conf", lbug::Value::Float(contract.confidence)),
            ],
        )
    }

    pub fn batch_insert_wikilink_to_note_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_wikilink_to_note_edges_on(&conn, edges)
    }

    /// Insert wikilink-to-note edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_wikilink_to_note_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (n:Note {uid: $nid}) \
                 CREATE (s)-[:WIKILINK_TO_NOTE {confidence: $conf, display: $disp}]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, note_uid, conf, display) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("disp", lbug::Value::String(display.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_wikilink_to_heading_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_wikilink_to_heading_edges_on(&conn, edges)
    }

    /// Insert wikilink-to-heading edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_wikilink_to_heading_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (h:Heading {uid: $hid}) \
                 CREATE (s)-[:WIKILINK_TO_HEADING {confidence: $conf, display: $disp}]->(h)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, head_uid, conf, display) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("hid", lbug::Value::String(head_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("disp", lbug::Value::String(display.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_tag_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_tag_edges_on(&conn, edges)
    }

    /// Insert note-tag edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_tag_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (t:Tag {uid: $tid}) \
                 CREATE (n)-[:NOTE_TAGGED_WITH]->(t)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, tag_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("tid", lbug::Value::String(tag_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_section_tag_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_section_tag_edges_on(&conn, edges)
    }

    /// Insert section-tag edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_section_tag_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (t:Tag {uid: $tid}) \
                 CREATE (s)-[:SECTION_TAGGED_WITH]->(t)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (section_uid, tag_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(section_uid.to_string())),
                    ("tid", lbug::Value::String(tag_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Upsert helpers (delete-then-create) ──────────────────────────────
    //
    // LadybugDB/Kuzu doesn't support MERGE or SET for most node types.
    // The established pattern (see `update_repo_sha`) is: read → DETACH
    // DELETE → re-CREATE. These helpers formalize this for node types
    // that need idempotent re-insertion (e.g. project materialization).

    /// Upsert a Note node. Deletes the existing Note (cascading headings,
    /// sections, and all incident edges) then re-inserts it.
    pub fn upsert_note(&self, note: &Note) -> Result<(), StoreError> {
        // delete_note_cascade is a no-op when the UID does not exist.
        self.delete_note_cascade(&note.uid)?;
        self.insert_note(note)
    }

    /// Upsert a Project node. DETACH DELETEs any existing node with the
    /// same UID **and** any existing node with the same name
    /// (case-insensitive), then re-creates it. The name-based cleanup
    /// prevents duplicate Project nodes when `instance_id` changes
    /// between materializer runs (which changes the UID).
    pub fn upsert_project(&self, project: &Project) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // Delete by exact UID (fast path — covers the common case).
        exec_params(
            &conn,
            "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
            vec![("uid", lbug::Value::String(project.uid.clone()))],
        )?;

        // Also delete any project with the same name regardless of UID.
        // LadybugDB has no toLower(), so we list all projects and delete
        // matches by UID in a second pass.
        let all = self.list_projects()?;
        let needle = project.name.to_lowercase();
        for existing in &all {
            if existing.uid != project.uid && existing.name.to_lowercase() == needle {
                exec_params(
                    &conn,
                    "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
                    vec![("uid", lbug::Value::String(existing.uid.clone()))],
                )?;
            }
        }

        self.insert_project(project)
    }

    /// Upsert a batch of sections. For each section, deletes it by UID
    /// (DETACH DELETE to remove incident edges) then re-inserts.
    pub fn batch_upsert_sections(&self, sections: &[Section]) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // Delete existing sections.
        let mut del_stmt = conn
            .prepare("MATCH (s:Section {uid: $uid}) DETACH DELETE s")
            .map_err(|e| StoreError::Query(format!("prepare delete: {e}")))?;
        for s in sections {
            conn.execute(
                &mut del_stmt,
                vec![("uid", lbug::Value::String(s.uid.clone()))],
            )
            .map_err(|e| StoreError::Query(format!("execute delete: {e}")))?;
        }

        // Re-insert.
        let mut ins_stmt = conn
            .prepare(
                "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
                 start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
                 word_count: $wc, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare insert: {e}")))?;
        for s in sections {
            conn.execute(
                &mut ins_stmt,
                vec![
                    ("uid", lbug::Value::String(s.uid.clone())),
                    ("nid", lbug::Value::String(s.note_uid.clone())),
                    (
                        "hid",
                        lbug::Value::String(s.heading_uid.clone().unwrap_or_default()),
                    ),
                    ("sl", lbug::Value::Int64(s.start_line as i64)),
                    ("el", lbug::Value::Int64(s.end_line as i64)),
                    ("th", lbug::Value::String(s.text_hash.clone())),
                    ("tc", lbug::Value::String(s.text_content.clone())),
                    ("wc", lbug::Value::Int64(s.word_count as i64)),
                    ("pr", lbug::Value::Double(s.pagerank_score.unwrap_or(0.0))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute insert: {e}")))?;
        }
        Ok(())
    }

    /// Delete a Note and everything that belongs to it: all Headings, all
    /// Sections, and every edge involving any of those nodes (both
    /// containment and cross-reference).
    ///
    /// LadybugDB's Cypher dialect supports `DETACH DELETE` which removes
    /// the node along with all its attached relationships in one shot —
    /// that's what makes this cascade tractable without enumerating every
    /// individual REL TABLE the new nodes participate in.
    ///
    /// This is the foundation of incremental updates: on every file
    /// modify, the watcher calls `delete_note_cascade(note_uid)` then
    /// re-inserts the freshly-parsed Note + descendants. UIDs are stable
    /// across edits (content_hash is in `Note.content_hash`, not in
    /// `Note.uid`), so any inbound wikilinks from other notes survive the
    /// cycle naturally — they get reattached to the same target_uid on
    /// the next reindex pass.
    pub fn delete_note_cascade(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // 1. Drop every Section under this note. DETACH removes the
        //    NOTE_HAS_SECTION, HEADING_HAS_SECTION, WIKILINK_TO_NOTE
        //    (incoming) and SECTION_TAGGED_WITH edges along with it.
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid})-[:NOTE_HAS_SECTION]->(s:Section) DETACH DELETE s",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 2. Drop every Heading under this note. DETACH removes
        //    NOTE_HAS_HEADING, HEADING_HAS_SECTION (already gone if its
        //    section was dropped above), HEADING_PARENT (both directions),
        //    and WIKILINK_TO_HEADING (incoming).
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid})-[:NOTE_HAS_HEADING]->(h:Heading) DETACH DELETE h",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 3. Drop the Note itself. DETACH removes VAULT_HAS_NOTE,
        //    NOTE_TAGGED_WITH, PROJECT_INCLUDES_NOTE, and any incoming
        //    WIKILINK_TO_NOTE edges from other notes' sections.
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid}) DETACH DELETE n",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 4. Drop any recorded unresolved-wikilink rows for this note so they
        //    do not linger after re-index (e.g. once the target note appears).
        self.delete_unresolved_wikilinks_for_note(note_uid)?;

        Ok(())
    }

    /// Cascade-delete a Vault and every Note belonging to it using bulk
    /// DETACH DELETE queries scoped by `vault_uid` — avoids the O(N) per-note
    /// query loop that was issuing 4 queries × N notes.
    ///
    /// Order of operations (within a single transaction):
    ///   1. Count notes (before deleting, so we can return the count).
    ///   2. Delete Sections via edge traversal (NOTE_HAS_SECTION).
    ///   3. Delete Headings via edge traversal (NOTE_HAS_HEADING).
    ///   4. Delete UnresolvedWikilinks via cross-node join on source_note_uid.
    ///   5. Delete all Note nodes (vault_uid property; DETACH removes
    ///      VAULT_HAS_NOTE, NOTE_TAGGED_WITH, PROJECT_INCLUDES_NOTE, and
    ///      all incoming WIKILINK_TO_NOTE / WIKILINK_TO_HEADING edges).
    ///   6. Delete Tag nodes belonging to this vault.
    ///   7. Delete the Vault node itself.
    ///
    /// `delete_note_cascade` is kept as-is for incremental single-note deletions.
    pub fn delete_vault_cascade(&self, vault_uid: &str) -> Result<usize, StoreError> {
        // Count notes before deletion so we can return the count.
        let count = {
            let conn = self.conn()?;
            let safe_vid = vault_uid.replace('\'', "\\'");
            let rows = conn
                .query(&format!(
                    "MATCH (n:Note) WHERE n.vault_uid = '{safe_vid}' RETURN count(n)"
                ))
                .map_err(|e| StoreError::Query(format!("count notes: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        let conn = self.begin_transaction()?;

        // 1. Delete all Sections under notes in this vault.
        exec_params(
            &conn,
            "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_SECTION]->(s:Section) DETACH DELETE s",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 2. Delete all Headings under notes in this vault.
        exec_params(
            &conn,
            "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_HEADING]->(h:Heading) DETACH DELETE h",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 3. Delete UnresolvedWikilinks whose source note belongs to this vault.
        //    Uses a cross-node join: LadybugDB supports `MATCH (a), (b) WHERE a.prop = b.prop`.
        //    Best-effort: silently skip if the table does not exist on older DBs.
        {
            let safe_vid = vault_uid.replace('\'', "\\'");
            if let Err(e) = conn.query(&format!(
                "MATCH (n:Note), (u:UnresolvedWikilink) \
                 WHERE n.vault_uid = '{safe_vid}' AND u.source_note_uid = n.uid \
                 DELETE u"
            )) {
                tracing::trace!("delete_vault_cascade: UnresolvedWikilink delete skipped: {e}");
            }
        }

        // 4. Delete all Note nodes (DETACH removes VAULT_HAS_NOTE, NOTE_TAGGED_WITH,
        //    PROJECT_INCLUDES_NOTE, and any incoming/outgoing wikilink edges).
        exec_params(
            &conn,
            "MATCH (n:Note {vault_uid: $vid}) DETACH DELETE n",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 5. Delete Tag nodes belonging to this vault.
        exec_params(
            &conn,
            "MATCH (t:Tag {vault_uid: $vid}) DETACH DELETE t",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 6. Delete the Vault node itself.
        exec_params(
            &conn,
            "MATCH (v:Vault {uid: $vid}) DETACH DELETE v",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        self.commit_transaction(&conn)?;
        Ok(count)
    }

    /// Batch insert REFERENCES_CODE edges from Note → Symbol. Each tuple
    /// is (note_uid, symbol_uid, confidence, source) where `source` is a
    /// short tag (`"name-match"`, `"code-block"`, `"annotation"`).
    pub fn batch_insert_note_to_symbol_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_to_symbol_edges_on(&conn, edges)
    }

    /// Insert note→symbol edges using an externally-provided connection
    /// (for transaction batching across many notes — avoids one fsync per call).
    pub fn batch_insert_note_to_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (s:Symbol {uid: $sid}) \
                 CREATE (n)-[:REFERENCES_CODE_NOTE_TO_SYMBOL \
                 {confidence: $conf, source: $source}]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (n_uid, s_uid, conf, src) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(n_uid.to_string())),
                    ("sid", lbug::Value::String(s_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("source", lbug::Value::String(src.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    /// Batch insert REFERENCES_CODE edges from Section → Symbol.
    pub fn batch_insert_section_to_symbol_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_section_to_symbol_edges_on(&conn, edges)
    }

    /// Insert section→symbol edges using an externally-provided connection
    /// (for transaction batching across many notes — avoids one fsync per call).
    pub fn batch_insert_section_to_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut stmt = conn
            .prepare(
                "MATCH (sec:Section {uid: $sid}), (sym:Symbol {uid: $symid}) \
                 CREATE (sec)-[:REFERENCES_CODE_SECTION_TO_SYMBOL \
                 {confidence: $conf, source: $source}]->(sym)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, sym_uid, conf, src) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("symid", lbug::Value::String(sym_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("source", lbug::Value::String(src.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    /// Delete all REFERENCES_CODE edges originating from a note and its
    /// sections. Called before re-emitting cross-domain edges to ensure
    /// idempotency.
    pub fn delete_cross_domain_edges_for_note(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::delete_cross_domain_edges_for_note_on(&conn, note_uid)
    }

    /// Delete cross-domain edges for a note using an externally-provided
    /// connection (for transaction batching across many notes).
    pub fn delete_cross_domain_edges_for_note_on(
        conn: &lbug::Connection<'_>,
        note_uid: &str,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "MATCH (n:Note {uid: $uid})-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->() DELETE r",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;
        // Section-level edges: find sections belonging to this note and
        // delete their outgoing REFERENCES_CODE edges.
        let section_uids: Vec<String> = {
            // LadybugDB does not support parameterized compound
            // property-match queries. Sanitize user-derived UIDs by
            // escaping single quotes to prevent Cypher injection.
            let safe_note_uid = note_uid.replace('\'', "\\'");
            let rows = conn
                .query(&format!(
                    "MATCH (n:Note {{uid: '{safe_note_uid}'}})-[:NOTE_HAS_SECTION]->(s:Section) RETURN s.uid"
                ))
                .map_err(|e| StoreError::Query(format!("query sections: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
            })
            .collect()
        };
        for s_uid in &section_uids {
            exec_params(
                conn,
                "MATCH (s:Section {uid: $uid})-[r:REFERENCES_CODE_SECTION_TO_SYMBOL]->() DELETE r",
                vec![("uid", lbug::Value::String(s_uid.clone()))],
            )?;
        }
        Ok(())
    }

    /// Delete all Symbol nodes that belong to a specific file (matching both
    /// `repo_uid` AND `file_path`). Uses `DETACH DELETE` so all incident edges
    /// (CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM, USES, ACCESSES, MEMBER_OF,
    /// FILE_HAS_SYMBOL, CROSS_REPO_LINK, REFERENCES_CODE_*) are automatically
    /// removed. Returns the count of deleted symbols.
    pub fn delete_symbols_in_file(
        &self,
        repo_uid: &str,
        file_path: &str,
    ) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        // LadybugDB does not support parameterized compound WHERE clauses.
        // Sanitize user-derived values by escaping single quotes.
        let safe_repo_uid = repo_uid.replace('\'', "\\'");
        let safe_file_path = file_path.replace('\'', "\\'");

        // Count first so we can report how many were deleted.
        let count: usize = {
            let rows = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo_uid}' AND s.file_path = '{safe_file_path}' RETURN count(s)"
                ))
                .map_err(|e| StoreError::Query(format!("count symbols: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        if count > 0 {
            // Single bulk DETACH DELETE instead of per-UID queries.
            conn.query(&format!(
                "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo_uid}' AND s.file_path = '{safe_file_path}' DETACH DELETE s"
            ))
            .map_err(|e| StoreError::Query(format!("delete symbols in file: {e}")))?;
        }

        Ok(count)
    }

    /// Delete a File node by its UID using `DETACH DELETE`, which removes all
    /// incident edges (REPO_HAS_FILE, FILE_HAS_SYMBOL) automatically.
    pub fn delete_file_node(&self, file_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (f:File {uid: $uid}) DETACH DELETE f",
            vec![("uid", lbug::Value::String(file_uid.to_string()))],
        )
    }

    /// Update `file_path` on every Symbol belonging to `repo_uid` that
    /// currently has `old_path`.  LadybugDB does not support `SET`, so each
    /// symbol is deleted and re-created with the new path while preserving all
    /// other fields.
    pub fn update_symbol_file_paths(
        &self,
        repo_uid: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), StoreError> {
        use crate::read::row_to_symbol;

        let conn = self.conn()?;

        let rows: Vec<_> = {
            let r = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{}' AND s.file_path = '{}' RETURN \
                     s.uid, s.name, s.kind, s.repo_uid, s.file_path, s.start_line, \
                     s.signature, s.summary, s.content_hash, s.pagerank_score",
                    repo_uid.replace('\'', "''"),
                    old_path.replace('\'', "''"),
                ))
                .map_err(|e| StoreError::Query(format!("query symbols: {e}")))?;
            r.collect()
        };

        for row in rows {
            let mut sym = row_to_symbol(&row)?;
            let old_uid = sym.uid.clone();
            sym.file_path = new_path.to_string();

            exec_params(
                &conn,
                "MATCH (s:Symbol {uid: $uid}) DETACH DELETE s",
                vec![("uid", lbug::Value::String(old_uid))],
            )?;

            self.insert_symbol_with_conn(&conn, &sym)?;
        }

        Ok(())
    }

    /// Update the `indexed_sha` field of a Repo node.
    pub fn update_repo_sha(&self, repo_uid: &str, new_sha: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;

        let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, r.instance_id, r.name";
        let rows: Vec<_> = conn
            .query(&format!(
                "MATCH (r:Repo {{uid: '{}'}}) RETURN {cols}",
                repo_uid.replace('\'', "''"),
            ))
            .map_err(|e| StoreError::Query(format!("query repo: {e}")))?
            .collect();

        let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;

        let uid = match row.first() {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo uid missing".to_string())),
        };
        let url = match row.get(1) {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo url missing".to_string())),
        };
        let staleness = match row.get(3) {
            Some(lbug::Value::Int64(n)) => *n,
            _ => 0,
        };
        let instance_id = match row.get(4) {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo instance_id missing".to_string())),
        };
        let name = match row.get(5) {
            Some(lbug::Value::String(s)) if !s.is_empty() => s.clone(),
            _ => String::new(),
        };

        exec_params(
            &conn,
            "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
            vec![("uid", lbug::Value::String(uid.clone()))],
        )?;

        exec_params(
            &conn,
            "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
             staleness_commits_behind: $scb, instance_id: $iid, name: $name})",
            vec![
                ("uid", lbug::Value::String(uid)),
                ("url", lbug::Value::String(url)),
                ("sha", lbug::Value::String(new_sha.to_string())),
                ("scb", lbug::Value::Int64(staleness)),
                ("iid", lbug::Value::String(instance_id)),
                ("name", lbug::Value::String(name)),
            ],
        )?;

        Ok(())
    }

    /// Update the `embedding` field of a Symbol node.
    ///
    /// LadybugDB does not support `SET`, so the symbol is read, deleted with
    /// DETACH DELETE, and re-inserted with the embedding set. This preserves
    /// all other fields. The embedding is stored as a JSON-encoded string in
    /// a separate sidecar structure; the Symbol node itself does not hold a
    /// native float-array column — instead the embedding is stored in the
    /// in-memory `Symbol.embedding` field, and callers that need persistence
    /// across sessions should use an `EmbeddingIndex` sidecar file.
    ///
    /// This method updates the in-graph Symbol node so that `list_all_symbols`
    /// can return embeddings without a separate sidecar.
    pub fn update_symbol_embedding(&self, uid: &str, embedding: &[f32]) -> Result<(), StoreError> {
        use crate::read::{SYMBOL_COLUMNS, row_to_symbol};

        let conn = self.conn()?;

        // Read the existing symbol.
        let q = format!("MATCH (s:Symbol {{uid: $uid}}) RETURN {SYMBOL_COLUMNS}");
        let mut stmt = conn
            .prepare(&q)
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        let mut result = conn
            .execute(
                &mut stmt,
                vec![("uid", lbug::Value::String(uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        let row = result.next().ok_or(StoreError::NotFound)?;
        let mut sym = row_to_symbol(&row)?;

        // Set the embedding in memory.
        sym.embedding = Some(embedding.to_vec());

        // Delete the existing node (DETACH removes all edges).
        exec_params(
            &conn,
            "MATCH (s:Symbol {uid: $uid}) DETACH DELETE s",
            vec![("uid", lbug::Value::String(uid.to_string()))],
        )?;

        // Re-insert with the embedding set. The Symbol struct carries
        // `embedding` but the CREATE statement used by
        // `insert_symbol_with_conn` does not include it (LadybugDB does
        // not support arbitrary array columns). The embedding is therefore
        // held only in the `Symbol` in-memory representation returned by
        // `list_all_symbols`; the on-disk node is refreshed with all other
        // fields preserved.
        self.insert_symbol_with_conn(&conn, &sym)?;

        Ok(())
    }

    /// Bulk-delete all Symbol and File nodes belonging to `repo_uid` using two
    /// DETACH DELETE queries instead of one per file. Called by `delete_repo_all_data`
    /// before a forced full re-index. `DETACH DELETE` removes all incident edges
    /// (FILE_HAS_SYMBOL, REPO_HAS_FILE, CALLS, IMPORTS, etc.) automatically.
    ///
    /// Returns `(file_count, symbol_count)` for logging.
    pub fn bulk_delete_repo_files_and_symbols(
        &self,
        repo_uid: &str,
    ) -> Result<(usize, usize), StoreError> {
        let rid = lbug::Value::String(repo_uid.to_string());

        let conn = self.begin_transaction()?;

        // Count before deleting so the caller can log what was removed.
        let sym_count: usize = {
            let mut stmt = conn
                .prepare("MATCH (s:Symbol) WHERE s.repo_uid = $rid RETURN count(s)")
                .map_err(|e| StoreError::Query(format!("prepare count symbols: {e}")))?;
            let rows = conn
                .execute(&mut stmt, vec![("rid", rid.clone())])
                .map_err(|e| StoreError::Query(format!("count symbols: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };
        let file_count: usize = {
            let mut stmt = conn
                .prepare("MATCH (f:File) WHERE f.repo_uid = $rid RETURN count(f)")
                .map_err(|e| StoreError::Query(format!("prepare count files: {e}")))?;
            let rows = conn
                .execute(&mut stmt, vec![("rid", rid.clone())])
                .map_err(|e| StoreError::Query(format!("count files: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        let mut stmt = conn
            .prepare("MATCH (s:Symbol) WHERE s.repo_uid = $rid DETACH DELETE s")
            .map_err(|e| StoreError::Query(format!("prepare delete symbols: {e}")))?;
        conn.execute(&mut stmt, vec![("rid", rid.clone())])
            .map_err(|e| StoreError::Query(format!("bulk delete symbols: {e}")))?;

        let mut stmt = conn
            .prepare("MATCH (f:File) WHERE f.repo_uid = $rid DETACH DELETE f")
            .map_err(|e| StoreError::Query(format!("prepare delete files: {e}")))?;
        conn.execute(&mut stmt, vec![("rid", rid)])
            .map_err(|e| StoreError::Query(format!("bulk delete files: {e}")))?;

        self.commit_transaction(&conn)?;
        Ok((file_count, sym_count))
    }

    /// Delete a Repo node (and its REPO_HAS_FILE edges) by UID.
    pub fn delete_repo_node(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )?;
        Ok(())
    }

    /// Delete all repo-scoped graph nodes that are NOT keyed off a stable,
    /// re-derivable UID and therefore would collide on a forced full re-index.
    ///
    /// `bulk_index_write` plain-`CREATE`s `Service` nodes (whose UID is derived
    /// from `repo_uid` + directory), and the contracts pass creates `Contract`
    /// nodes. Re-running `index --force` regenerates the same UIDs, so without
    /// clearing them first the second run trips LadybugDB's primary-key
    /// uniqueness constraint (`Found duplicated primary key value svc:...`).
    ///
    /// `DETACH DELETE` also removes incident `SERVICE_HAS_SYMBOL`,
    /// `IMPLEMENTS_CONTRACT`, and `SUPERSEDES`/`DEPENDS_ON`/`CAUSED_BY`/
    /// `RELATES_TO` edges. `Symbol`/`File` nodes are cleared separately by the
    /// per-file `delete_symbols_in_file` / `delete_file_node` path. Idempotent:
    /// a no-op for repos with no services/contracts.
    pub fn clear_repo_derived_nodes(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        // Service nodes for this repo.
        exec_params(
            &conn,
            "MATCH (s:Service {repo_uid: $uid}) DETACH DELETE s",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )?;
        // Contract nodes for this repo (table may not exist on older DBs).
        if let Err(e) = exec_params(
            &conn,
            "MATCH (c:Contract {repo_uid: $uid}) DETACH DELETE c",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        ) {
            tracing::trace!("clear_repo_derived_nodes: Contract delete skipped: {e}");
        }
        Ok(())
    }

    /// Cascade-delete every graph row whose `instance_id` matches `id`:
    /// all Repos (with their files, symbols, services, contracts), all
    /// Vaults (with their notes/headings/sections via
    /// `delete_vault_cascade`), and all Projects. Composes the same
    /// per-Repo cleanup that `index --force` uses, so no novel write
    /// paths are introduced. Idempotent: returns zero counts on a clean
    /// DB. Useful for recovering from a misconfigured `instance merge`
    /// that left an orphan instance ID behind.
    pub fn purge_instance(&self, id: &str) -> Result<PurgeInstanceResult, StoreError> {
        let mut result = PurgeInstanceResult::default();

        // Repos owned by this instance — cascade delete every File,
        // Symbol, Service, and Contract that hangs off each one before
        // dropping the Repo node itself.
        let repos = self.list_repos(Some(id))?;
        for r in &repos {
            let (files, syms) = self.bulk_delete_repo_files_and_symbols(&r.uid)?;
            self.clear_repo_derived_nodes(&r.uid)?;
            self.delete_repo_node(&r.uid)?;
            result.files += files;
            result.symbols += syms;
        }
        result.repos = repos.len();

        // Vaults owned by this instance — cascade Note/Heading/Section.
        let vaults = self.list_vaults(Some(id))?;
        for v in &vaults {
            let notes = self.delete_vault_cascade(&v.uid)?;
            result.notes += notes;
        }
        result.vaults = vaults.len();

        // Projects owned by this instance — single DETACH DELETE each.
        let projects = self.list_projects()?;
        for p in &projects {
            if p.instance_id == id {
                let conn = self.conn()?;
                exec_params(
                    &conn,
                    "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
                    vec![("uid", lbug::Value::String(p.uid.clone()))],
                )?;
                result.projects += 1;
            }
        }

        // Orphan sweep: a partial `instance merge` can drop the Repo or
        // Vault node while leaving its child Symbol/File/Service/Note
        // rows behind. Those children still encode the source instance
        // in their UID prefix, so we can find and drop them even after
        // the parent is gone. Order matters only for telemetry — every
        // statement is `DETACH DELETE` so incident edges are cleaned.
        for (label, prefix) in [
            ("Symbol", format!("sym:repo:{id}:")),
            ("File", format!("file:repo:{id}:")),
            ("Service", format!("svc:repo:{id}:")),
            ("Note", format!("note:vlt:{id}:")),
            ("Heading", format!("head:note:vlt:{id}:")),
            ("Section", format!("sec:note:vlt:{id}:")),
            ("Tag", format!("tag:vlt:{id}:")),
            // Defensive: also catch Repo/Vault/Project rows that the
            // registry-walk above missed (e.g. stale rows whose
            // instance_id column was scrambled but whose UID is intact).
            ("Repo", format!("repo:{id}:")),
            ("Vault", format!("vlt:{id}:")),
            ("Project", format!("proj:{id}:")),
        ] {
            result.orphans_swept += self.sweep_orphan_nodes(label, &prefix)?;
        }

        Ok(result)
    }

    /// Count and DETACH DELETE every node of `label` whose `uid` starts
    /// with `prefix`. Returns the number of rows removed. Idempotent.
    /// Used by [`purge_instance`] to clean up orphans left by a
    /// partial `instance merge` that already dropped the parent
    /// Repo/Vault node.
    fn sweep_orphan_nodes(&self, label: &str, prefix: &str) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let query =
            format!("MATCH (n:{label}) WHERE n.uid STARTS WITH $p DETACH DELETE n RETURN count(n)");
        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| StoreError::Query(format!("prepare sweep {label} orphans: {e}")))?;
        let rows = conn
            .execute(
                &mut stmt,
                vec![("p", lbug::Value::String(prefix.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("sweep {label} orphans: {e}")))?;
        let count = rows
            .filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0);
        Ok(count)
    }

    /// Insert a single CROSS_REPO_LINK edge between two Symbol nodes.
    pub fn insert_cross_repo_link(
        &self,
        from_uid: &str,
        to_uid: &str,
        confidence: f32,
        link_type: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (a:Symbol {uid: $from}), (b:Symbol {uid: $to}) \
             CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt}]->(b)",
            vec![
                ("from", lbug::Value::String(from_uid.to_string())),
                ("to", lbug::Value::String(to_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
                ("lt", lbug::Value::String(link_type.to_string())),
            ],
        )
    }

    pub fn batch_insert_project_note_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "MATCH (p:Project {uid: $pid}), (n:Note {uid: $nid}) \
                 CREATE (p)-[:PROJECT_INCLUDES_NOTE {confidence: 1.0}]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (project_uid, note_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("pid", lbug::Value::String(project_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_project_symbol_edges(
        &self,
        project_uid: &str,
        symbol_uids: &[String],
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "MATCH (p:Project {uid: $pid}), (s:Symbol {uid: $sid}) \
                 CREATE (p)-[:PROJECT_INCLUDES_SYMBOL {confidence: $conf}]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for sym_uid in symbol_uids {
            conn.execute(
                &mut stmt,
                vec![
                    ("pid", lbug::Value::String(project_uid.to_string())),
                    ("sid", lbug::Value::String(sym_uid.clone())),
                    ("conf", lbug::Value::Double(confidence as f64)),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_project_component_edge(
        &self,
        parent_uid: &str,
        child_uid: &str,
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (p:Project {uid: $pid}), (c:Project {uid: $cid}) \
             CREATE (p)-[:PROJECT_HAS_COMPONENT {confidence: $conf}]->(c)",
            vec![
                ("pid", lbug::Value::String(parent_uid.to_string())),
                ("cid", lbug::Value::String(child_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
            ],
        )
    }

    pub fn insert_project_parent_edge(
        &self,
        child_uid: &str,
        parent_uid: &str,
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (c:Project {uid: $cid}), (p:Project {uid: $pid}) \
             CREATE (c)-[:PROJECT_HAS_PARENT {confidence: $conf}]->(p)",
            vec![
                ("cid", lbug::Value::String(child_uid.to_string())),
                ("pid", lbug::Value::String(parent_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
            ],
        )
    }

    /// Delete all outgoing project edges for the given project UID.
    /// Idempotent — silently ignores errors (table may not exist on first run).
    pub fn delete_project_edges(&self, project_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        for edge_type in &[
            "PROJECT_INCLUDES_NOTE",
            "PROJECT_INCLUDES_SYMBOL",
            "PROJECT_HAS_COMPONENT",
            "PROJECT_HAS_PARENT",
        ] {
            let q = format!("MATCH (p:Project {{uid: $uid}})-[r:{edge_type}]->() DELETE r");
            if let Ok(mut stmt) = conn.prepare(&q) {
                let _ = conn.execute(
                    &mut stmt,
                    vec![("uid", lbug::Value::String(project_uid.to_string()))],
                );
            }
        }
        Ok(())
    }

    /// Count notes belonging to a vault.
    fn vault_note_count(&self, vault_uid: &str) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let safe_vid = vault_uid.replace('\'', "\\'");
        let rows = conn
            .query(&format!(
                "MATCH (n:Note) WHERE n.vault_uid = '{safe_vid}' RETURN count(n)"
            ))
            .map_err(|e| StoreError::Query(format!("count notes: {e}")))?;
        Ok(rows
            .filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0))
    }

    /// Migrate all child nodes (notes, headings, sections, tags) from one
    /// vault to a new vault with a different UID and instance_id. The old
    /// vault is cascade-deleted and a new vault is created in its place,
    /// preserving all content.
    ///
    /// This uses the LadybugDB-compatible DETACH DELETE + re-CREATE pattern
    /// since SET is not supported for property updates.
    pub fn reparent_vault(
        &self,
        old_vault_uid: &str,
        new_vault_uid: &str,
        new_instance_id: &str,
    ) -> Result<ReparentVaultResult, StoreError> {
        // 1. Read the old vault metadata.
        let old_vault = self
            .list_vaults(None)?
            .into_iter()
            .find(|v| v.uid == old_vault_uid)
            .ok_or_else(|| {
                StoreError::Query(format!("vault not found: {old_vault_uid}"))
            })?;

        // 2. Read all children and edges before deletion.
        let notes = self.list_notes(Some(old_vault_uid))?;
        let headings = self.list_headings_by_vault(old_vault_uid)?;
        let sections = self.list_sections_by_vault(old_vault_uid)?;
        let tags = self.list_tags(Some(old_vault_uid))?;

        // Capture note-tag edges before cascade destroys them.
        let note_tag_edges: Vec<(String, String)> = {
            let conn = self.conn()?;
            let q = "MATCH (n:Note {vault_uid: $vid})-[:NOTE_TAGGED_WITH]->(t:Tag) \
                     RETURN n.uid, t.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(old_vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            result
                .filter_map(|row| {
                    let nuid = crate::read::extract_string(&row, 0).ok()?;
                    let tuid = crate::read::extract_string(&row, 1).ok()?;
                    Some((nuid, tuid))
                })
                .collect()
        };

        // Capture section-tag edges before cascade destroys them.
        let section_tag_edges: Vec<(String, String)> = {
            let conn = self.conn()?;
            let q = "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_SECTION]->(s:Section)-[:SECTION_TAGGED_WITH]->(t:Tag) \
                     RETURN s.uid, t.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(format!("prepare section_tag edges: {e}")))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(old_vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute section_tag edges: {e}")))?;
            result
                .filter_map(|row| {
                    let suid = crate::read::extract_string(&row, 0).ok()?;
                    let tuid = crate::read::extract_string(&row, 1).ok()?;
                    Some((suid, tuid))
                })
                .collect()
        };

        let result = ReparentVaultResult {
            notes_migrated: notes.len(),
            headings_migrated: headings.len(),
            sections_migrated: sections.len(),
            tags_migrated: tags.len(),
        };

        // 3. Delete old vault and all its children.
        self.delete_vault_cascade(old_vault_uid)?;

        // 4. Create new vault with updated UID and instance_id.
        self.insert_vault(&Vault {
            uid: new_vault_uid.to_string(),
            name: old_vault.name,
            root_path: old_vault.root_path,
            instance_id: new_instance_id.to_string(),
        })?;

        // 5. Re-insert notes with updated vault_uid.
        let reparented_notes: Vec<Note> = notes
            .into_iter()
            .map(|n| Note {
                vault_uid: new_vault_uid.to_string(),
                ..n
            })
            .collect();
        self.batch_insert_notes(&reparented_notes)?;

        // Re-create VAULT_HAS_NOTE edges.
        let vault_note_edges: Vec<(&str, &str)> = reparented_notes
            .iter()
            .map(|n| (new_vault_uid, n.uid.as_str()))
            .collect();
        self.batch_insert_vault_note_edges(&vault_note_edges)?;

        // 6. Re-insert headings (note_uid stays the same).
        self.batch_insert_headings(&headings)?;

        // Re-create NOTE_HAS_HEADING edges.
        let note_heading_edges: Vec<(&str, &str)> = headings
            .iter()
            .map(|h| (h.note_uid.as_str(), h.uid.as_str()))
            .collect();
        self.batch_insert_note_heading_edges(&note_heading_edges)?;

        // 7. Re-insert sections (note_uid stays the same).
        self.batch_insert_sections(&sections)?;

        // Re-create NOTE_HAS_SECTION edges.
        let note_section_edges: Vec<(&str, &str)> = sections
            .iter()
            .map(|s| (s.note_uid.as_str(), s.uid.as_str()))
            .collect();
        self.batch_insert_note_section_edges(&note_section_edges)?;

        // Re-create HEADING_HAS_SECTION edges where applicable.
        let heading_section_edges: Vec<(&str, &str)> = sections
            .iter()
            .filter_map(|s| {
                s.heading_uid
                    .as_ref()
                    .map(|huid| (huid.as_str(), s.uid.as_str()))
            })
            .collect();
        if !heading_section_edges.is_empty() {
            self.batch_insert_heading_section_edges(&heading_section_edges)?;
        }

        // 8. Re-insert tags with updated vault_uid.
        let reparented_tags: Vec<Tag> = tags
            .into_iter()
            .map(|t| Tag {
                vault_uid: new_vault_uid.to_string(),
                ..t
            })
            .collect();
        self.batch_insert_tags(&reparented_tags)?;

        // Re-create NOTE_TAGGED_WITH edges.
        let nt_edges: Vec<(&str, &str)> = note_tag_edges
            .iter()
            .map(|(nuid, tuid)| (nuid.as_str(), tuid.as_str()))
            .collect();
        if !nt_edges.is_empty() {
            self.batch_insert_note_tag_edges(&nt_edges)?;
        }

        // Re-create SECTION_TAGGED_WITH edges.
        let st_edges: Vec<(&str, &str)> = section_tag_edges
            .iter()
            .map(|(suid, tuid)| (suid.as_str(), tuid.as_str()))
            .collect();
        if !st_edges.is_empty() {
            self.batch_insert_section_tag_edges(&st_edges)?;
        }

        Ok(result)
    }

    /// Rewrite `instance_id` on all Vault, Repo, and Project nodes that
    /// match `from` to `to`. Returns a [`MergeResult`] with counts and
    /// details about any vaults whose notes were unlinked during collision
    /// resolution.
    ///
    /// Uses the LadybugDB-compatible DETACH DELETE + re-CREATE pattern
    /// since SET is not supported for property updates.
    pub fn merge_instance_ids(&self, from: &str, to: &str) -> Result<MergeResult, StoreError> {
        let mut vault_count = 0usize;
        let mut repo_count = 0usize;
        let mut project_count = 0usize;
        let mut unlinked: Vec<UnlinkedVault> = Vec::new();

        // Build a map of target-instance vaults keyed by root_path so we
        // can detect collisions and compare child counts.
        let target_vaults: std::collections::HashMap<String, Vault> = self
            .list_vaults(None)?
            .into_iter()
            .filter(|v| v.instance_id == to)
            .map(|v| (v.root_path.clone(), v))
            .collect();

        for v in self.list_vaults(None)? {
            if v.instance_id == from {
                let root_path = v.root_path.clone();
                let new_uid = vault_uid(to, &root_path);

                if let Some(target) = target_vaults.get(&root_path) {
                    // Collision: two vaults with the same root_path in
                    // different instances. Keep whichever has more notes.
                    let source_count = self.vault_note_count(&v.uid)?;
                    let target_count = self.vault_note_count(&target.uid)?;

                    if source_count > target_count {
                        // Source wins — delete target (intentional discard),
                        // then reparent source to preserve its notes.
                        let target_dropped = self.delete_vault_cascade(&target.uid)?;
                        self.reparent_vault(&v.uid, &new_uid, to)?;
                        if target_dropped > 0 {
                            unlinked.push(UnlinkedVault {
                                root_path,
                                notes_removed: target_dropped,
                            });
                        }
                    } else {
                        // Target wins — drop source (intentional discard).
                        let source_dropped = self.delete_vault_cascade(&v.uid)?;
                        if source_dropped > 0 {
                            unlinked.push(UnlinkedVault {
                                root_path,
                                notes_removed: source_dropped,
                            });
                        }
                    }
                } else {
                    // No collision — reparent source to preserve its notes.
                    self.reparent_vault(&v.uid, &new_uid, to)?;
                }
                vault_count += 1;
            }
        }
        for r in self.list_repos(None)? {
            if r.instance_id == from {
                let conn = self.conn()?;
                exec_params(
                    &conn,
                    "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
                    vec![("uid", lbug::Value::String(r.uid.clone()))],
                )?;
                self.insert_repo(&Repo {
                    uid: repo_uid(to, &r.url),
                    instance_id: to.to_string(),
                    ..r
                })?;
                repo_count += 1;
            }
        }
        for p in self.list_projects()? {
            if p.instance_id == from {
                self.upsert_project(&Project {
                    uid: project_uid(to, &p.name),
                    instance_id: to.to_string(),
                    ..p
                })?;
                project_count += 1;
            }
        }
        Ok(MergeResult {
            vaults: vault_count,
            repos: repo_count,
            projects: project_count,
            unlinked,
        })
    }
}

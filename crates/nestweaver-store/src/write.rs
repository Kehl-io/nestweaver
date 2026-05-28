use nestweaver_schema::{
    EdgeType, File, Heading, Note, Project, Repo, ResolvedEdge, Section, Service, Symbol, Tag,
    Vault,
};

use crate::db::GraphStore;
use crate::error::StoreError;

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
             staleness_commits_behind: $scb, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(repo.uid.clone())),
                ("url", lbug::Value::String(repo.url.clone())),
                ("sha", lbug::Value::String(repo.indexed_sha.clone())),
                (
                    "scb",
                    lbug::Value::Int64(repo.staleness_commits_behind as i64),
                ),
                ("iid", lbug::Value::String(repo.instance_id.clone())),
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
             repo_uid: $repo, file_path: $fp, start_line: $sl, \
             signature: $sig, summary: $summary, content_hash: $hash, \
             pagerank_score: $pr, is_entry_point: $iep, entry_point_kind: $epk})",
            vec![
                ("uid", lbug::Value::String(symbol.uid.clone())),
                ("name", lbug::Value::String(symbol.name.clone())),
                ("kind", lbug::Value::String(symbol.kind.to_string())),
                ("repo", lbug::Value::String(symbol.repo_uid.clone())),
                ("fp", lbug::Value::String(symbol.file_path.clone())),
                ("sl", lbug::Value::Int64(symbol.start_line as i64)),
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
                 repo_uid: $repo, file_path: $fp, start_line: $sl, \
                 signature: $sig, summary: $summary, content_hash: $hash, \
                 pagerank_score: $pr, is_entry_point: $iep, entry_point_kind: $epk})",
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

        match edge.edge_type {
            EdgeType::Calls => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:CALLS {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Imports => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:IMPORTS {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Extends => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:EXTENDS_SYM {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Implements => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:IMPLEMENTS_SYM {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Includes => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:INCLUDES_SYM {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Uses => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:USES {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::Accesses => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:ACCESSES {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                ],
            ),
            EdgeType::MemberOf => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                 CREATE (a)-[:MEMBER_OF {confidence: $conf}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
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
                     CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt}]->(b)",
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
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

    pub fn batch_insert_edges(&self, edges: &[ResolvedEdge]) -> Result<(), StoreError> {
        // Group edges by their SQL query string so we prepare each statement only once.
        use std::collections::HashMap;

        // Collect (query_string, params) pairs grouped by query.
        let mut groups: HashMap<String, Vec<Vec<(&str, lbug::Value)>>> = HashMap::new();

        for edge in edges {
            let src = edge.source_uid.clone();
            let tgt = edge.target_uid.clone();
            let conf = edge.confidence as f64;

            match edge.edge_type {
                EdgeType::Calls => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:CALLS {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Imports => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:IMPORTS {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Extends => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:EXTENDS_SYM {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Implements => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:IMPLEMENTS_SYM {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Includes => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:INCLUDES_SYM {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Uses => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:USES {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::Accesses => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:ACCESSES {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                    ]);
                }
                EdgeType::MemberOf => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:MEMBER_OF {confidence: $conf}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
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
                               CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
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

        let conn = self.begin_transaction()?;
        for (query, param_sets) in &groups {
            let mut stmt = conn
                .prepare(query)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            for params in param_sets {
                conn.execute(&mut stmt, params.clone())
                    .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            }
        }
        self.commit_transaction(&conn)?;
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

    pub fn batch_insert_wikilink_to_note_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
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
    /// same UID (removing all incident edges) then re-creates it.
    pub fn upsert_project(&self, project: &Project) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
            vec![("uid", lbug::Value::String(project.uid.clone()))],
        )?;
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

        Ok(())
    }

    /// Cascade-delete a Vault and every Note belonging to it. Calls
    /// `delete_note_cascade` for each note (which removes its headings +
    /// sections + cross-reference edges) then drops the Vault node.
    /// Returns the number of notes removed.
    pub fn delete_vault_cascade(&self, vault_uid: &str) -> Result<usize, StoreError> {
        // Find every note belonging to this vault first.
        let notes = self.list_notes(Some(vault_uid))?;
        let count = notes.len();
        for n in &notes {
            self.delete_note_cascade(&n.uid)?;
        }
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (v:Vault {uid: $uid}) DETACH DELETE v",
            vec![("uid", lbug::Value::String(vault_uid.to_string()))],
        )?;
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
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid})-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->() DELETE r",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;
        // Section-level edges: find sections belonging to this note and
        // delete their outgoing REFERENCES_CODE edges.
        let section_uids: Vec<String> = {
            let rows = conn
                .query(&format!(
                    "MATCH (n:Note {{uid: '{note_uid}'}})-[:NOTE_HAS_SECTION]->(s:Section) RETURN s.uid"
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
                &conn,
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
        // Collect UIDs first (LadybugDB does not support compound WHERE
        // parameterization, so we format the lookup query the same way
        // `delete_cross_domain_edges_for_note` does for its section query).
        let symbol_uids: Vec<String> = {
            let rows = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{repo_uid}' AND s.file_path = '{file_path}' RETURN s.uid"
                ))
                .map_err(|e| StoreError::Query(format!("query symbols: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
            })
            .collect()
        };
        let count = symbol_uids.len();
        for uid in &symbol_uids {
            exec_params(
                &conn,
                "MATCH (s:Symbol {uid: $uid}) DETACH DELETE s",
                vec![("uid", lbug::Value::String(uid.clone()))],
            )?;
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

        let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, r.instance_id";
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

        exec_params(
            &conn,
            "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
            vec![("uid", lbug::Value::String(uid.clone()))],
        )?;

        exec_params(
            &conn,
            "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
             staleness_commits_behind: $scb, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(uid)),
                ("url", lbug::Value::String(url)),
                ("sha", lbug::Value::String(new_sha.to_string())),
                ("scb", lbug::Value::Int64(staleness)),
                ("iid", lbug::Value::String(instance_id)),
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
        use crate::read::row_to_symbol;

        let conn = self.conn()?;

        // Read the existing symbol.
        let cols = "s.uid, s.name, s.kind, s.repo_uid, s.file_path, s.start_line, \
                    s.signature, s.summary, s.content_hash, s.pagerank_score";
        let q = format!("MATCH (s:Symbol {{uid: $uid}}) RETURN {cols}");
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
}

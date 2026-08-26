pub mod artifact_envelope;
pub mod cache;
pub mod db;
pub mod durable_sidecar;
pub mod error;
pub mod generation;
pub mod git_activity_sidecar;
pub mod index_publication;
pub mod ranking;
pub mod read;
pub mod regex;
pub mod regex_index;
pub mod search;
pub mod tantivy_index;
pub mod traverse;
pub mod write;
pub mod zstd;

pub use db::{
    EmbeddingIndexOccupancy, EmbeddingIndexReconciliation, EmbeddingSnapshotLease,
    EmbeddingSnapshotState, GraphStore, IndexPublicationLease, PublicationIdentity,
};
pub use error::{CancelReason, StoreError};

/// Re-export the LadybugDB connection type so callers can use transactional
/// APIs (`begin_transaction` / `commit_transaction`) and `_on` method variants
/// without depending on `lbug` directly.
pub use lbug::Connection as DbConnection;
pub use ranking::{
    DEFAULT_GIT_ACTIVITY_WEIGHT, GIT_ACTIVITY_MULT_MAX, GIT_ACTIVITY_MULT_MIN, GraphScope,
    PathDeboostRule, QueryIntent, SEED_PATH_FACTOR_MAX, SEED_PATH_FACTOR_MIN, ScopedEdgeQuery,
    SeedResolutionConfig, default_kind_priority, detect_intent, git_activity_multiplier,
};
pub use read::{
    BacklinkRow, BrokenWikilinkRow, CodeEdge, CodeGraph, CrossRepoRef, NoteLite, SymbolBasic,
};
pub use regex::{
    CANDIDATE_CAP, DEFAULT_MAX_MILLIS, FileCount, PatternCount, RegexMatch, RegexSearchResult,
    TrigramRefreshStats,
};
pub use regex_index::{
    REGEX_INDEX_SCHEMA_VERSION, REGEX_TOKENIZER_FINGERPRINT, RegexIndex, RegexShardMetadata,
};
pub use search::{
    EMBED_CHECKPOINT_INTERVAL, EmbeddingFlushCheckpoint, EmbeddingIndex, SearchResult,
};
pub use tantivy_index::{
    PRF_EXPANSION_TERMS, PRF_EXPANSION_WEIGHT, PRF_MAX_QUERY_TERMS, PRF_TOP_K,
    SEARCH_PRESENTATION_LIMIT_MAX, SearchHit, SearchLogicalIdentity, TantivyError, TantivyIndex,
};
pub use traverse::{
    DEFAULT_IMPACT_THRESHOLD, IMPACT_EDGE_TYPES, ImpactEdge, ImpactNode, ImpactResult,
    ImpactSnapshot,
};
pub use write::{
    CONTRACT_DERIVATION_FAILED_PREFIX, DeleteProjectCascadeError, DeleteProjectCascadeOutcome,
    DeleteRepoCascadeOutcome, DeleteVaultCascadeOutcome, DiscardedVault, InstanceProjectRecovery,
    InstanceRepoRecovery, InstanceUidHandoff, InstanceUidHandoffIdentity, InstanceUidMigrationPlan,
    InstanceUidRemap, InstanceUidRemapPlanState, InstanceVaultRecovery, MergeResult,
    MutationDisposition, MutationFailure, MutationOutcome, ProjectMutationDisposition,
    PurgeInstanceResult,
};

#[cfg(test)]
mod tests {
    use nestweaver_schema::{
        EdgeType, File, Repo, ResolvedEdge, Service, Symbol, SymbolKind, Tag, Vault, Visibility,
    };

    use super::{GraphStore, InstanceUidRemap, ProjectMutationDisposition};

    fn make_repo(uid: &str) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: format!("https://github.com/example/{uid}"),
            indexed_sha: "abc123".to_string(),
            staleness_commits_behind: 0,
            instance_id: "inst-1".to_string(),
            name: None,
            root_path: None,
        }
    }

    fn make_file(uid: &str, repo_uid: &str) -> File {
        File {
            uid: uid.to_string(),
            path: format!("src/{uid}.rs"),
            repo_uid: repo_uid.to_string(),
            content_hash: "hash1".to_string(),
        }
    }

    fn make_symbol(uid: &str, name: &str, repo_uid: &str, file_path: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: file_path.to_string(),
            start_line: 10,
            end_line: 25,
            signature: format!("fn {name}()"),
            summary: Some(format!("Does {name} things")),
            content_hash: "contenthash".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    #[test]
    fn vault_cascade_outcome_distinguishes_noop_empty_and_tag_only_deletions() {
        let store = GraphStore::in_memory().unwrap();

        let no_op = store
            .delete_vault_cascade_with_outcome("vlt:missing")
            .unwrap();
        assert_eq!(no_op.notes_deleted, 0);
        assert!(!no_op.changed);

        store
            .insert_vault(&Vault {
                uid: "vlt:empty".to_string(),
                name: "empty".to_string(),
                root_path: "/missing/empty".to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let empty = store
            .delete_vault_cascade_with_outcome("vlt:empty")
            .unwrap();
        assert_eq!(empty.notes_deleted, 0);
        assert!(empty.changed);

        store
            .insert_tag(&Tag {
                uid: "tag:vlt:tag-only:orphan".to_string(),
                vault_uid: "vlt:tag-only".to_string(),
                name: "orphan".to_string(),
            })
            .unwrap();
        let tag_only = store
            .delete_vault_cascade_with_outcome("vlt:tag-only")
            .unwrap();
        assert_eq!(tag_only.notes_deleted, 0);
        assert!(tag_only.changed);
        assert!(store.list_tags(Some("vlt:tag-only")).unwrap().is_empty());
    }

    #[test]
    fn project_cascade_is_atomic_and_removes_every_incident_project_edge() {
        use nestweaver_schema::{Note, NoteKind, Project};

        let store = GraphStore::in_memory().unwrap();
        for (uid, name) in [
            ("proj:test:parent", "Parent"),
            ("proj:test:target", "Target"),
            ("proj:test:child", "Child"),
        ] {
            store
                .insert_project(&Project {
                    uid: uid.to_string(),
                    name: name.to_string(),
                    summary: None,
                    instance_id: "test".to_string(),
                })
                .unwrap();
        }
        store
            .insert_note(&Note {
                uid: "note:project-delete".to_string(),
                vault_uid: "vlt:project-delete".to_string(),
                file_path: "project-delete.md".to_string(),
                title: "Project delete".to_string(),
                note_kind: NoteKind::General,
                word_count: 2,
                content_hash: "note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_symbol(&make_symbol(
                "sym:project-delete",
                "project_delete",
                "repo:project-delete",
                "src/lib.rs",
            ))
            .unwrap();

        store
            .batch_insert_project_note_edges(&[("proj:test:target", "note:project-delete")])
            .unwrap();
        store
            .batch_insert_project_symbol_edges(
                "proj:test:target",
                &["sym:project-delete".to_string()],
                1.0,
            )
            .unwrap();
        store
            .insert_project_component_edge("proj:test:parent", "proj:test:target", 1.0)
            .unwrap();
        store
            .insert_project_component_edge("proj:test:target", "proj:test:child", 1.0)
            .unwrap();
        store
            .insert_project_parent_edge("proj:test:target", "proj:test:parent", 1.0)
            .unwrap();
        store
            .insert_project_parent_edge("proj:test:child", "proj:test:target", 1.0)
            .unwrap();
        let conn = store.conn().unwrap();
        conn.query("CREATE REL TABLE FUTURE_PROJECT_TO_NOTE(FROM Project TO Note, marker STRING)")
            .unwrap();
        conn.query(
            "MATCH (p:Project {uid: 'proj:test:target'}), \
             (n:Note {uid: 'note:project-delete'}) \
             CREATE (p)-[:FUTURE_PROJECT_TO_NOTE {marker: 'future'}]->(n)",
        )
        .unwrap();

        let outcome = store
            .delete_project_cascade_with_outcome("proj:test:target")
            .unwrap();

        assert_eq!(outcome.disposition, ProjectMutationDisposition::Changed);
        assert_eq!(outcome.project_uid, "proj:test:target");
        assert_eq!(outcome.project_name.as_deref(), Some("Target"));
        assert_eq!(store.list_projects().unwrap().len(), 2);
        for edge_type in [
            "PROJECT_INCLUDES_NOTE",
            "PROJECT_INCLUDES_SYMBOL",
            "PROJECT_HAS_COMPONENT",
            "PROJECT_HAS_PARENT",
            "FUTURE_PROJECT_TO_NOTE",
        ] {
            let rows = conn
                .query(&format!("MATCH ()-[r:{edge_type}]->() RETURN count(r)"))
                .unwrap();
            let count = rows
                .filter_map(|row| match row.first() {
                    Some(lbug::Value::Int64(value)) => Some(*value),
                    _ => None,
                })
                .next()
                .unwrap_or_default();
            assert_eq!(count, 0, "{edge_type} survived the project delete");
        }
        let surviving_project_uids: std::collections::HashSet<_> = store
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|project| project.uid)
            .collect();
        assert_eq!(
            surviving_project_uids,
            std::collections::HashSet::from([
                "proj:test:parent".to_string(),
                "proj:test:child".to_string(),
            ])
        );
        for (label, uid) in [
            ("Note", "note:project-delete"),
            ("Symbol", "sym:project-delete"),
        ] {
            let mut rows = conn
                .query(&format!(
                    "MATCH (n:{label} {{uid: '{uid}'}}) RETURN count(n)"
                ))
                .unwrap();
            assert_eq!(
                rows.next()
                    .and_then(|row| match row.first() {
                        Some(lbug::Value::Int64(count)) => Some(*count),
                        _ => None,
                    })
                    .unwrap_or_default(),
                1,
                "unrelated {label} {uid} did not survive"
            );
        }

        let missing = store
            .delete_project_cascade_with_outcome("proj:test:missing")
            .unwrap();
        assert_eq!(
            missing.disposition,
            ProjectMutationDisposition::ConfirmedUnchanged
        );
        assert_eq!(missing.project_uid, "proj:test:missing");
        assert_eq!(missing.project_name, None);
    }

    fn make_service(uid: &str, repo_uid: &str) -> Service {
        Service {
            uid: uid.to_string(),
            name: format!("Service-{uid}"),
            repo_uid: repo_uid.to_string(),
            summary: Some("A service".to_string()),
            summary_hash: Some("shash".to_string()),
            embedding: None,
        }
    }

    fn test_store() -> GraphStore {
        GraphStore::in_memory().unwrap()
    }

    #[test]
    fn create_and_insert_repo() {
        let store = test_store();
        let repo = make_repo("repo-1");
        store.insert_repo(&repo).unwrap();

        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].uid, "repo-1");
        assert_eq!(repos[0].url, "https://github.com/example/repo-1");
    }

    #[test]
    fn removed_repositories_do_not_leak_contract_failure_or_migration_debt() {
        let store = test_store();
        let repo = make_repo("repo-removed-debt");
        store.insert_repo(&repo).unwrap();
        store
            .set_contract_derivation_failed(&repo.uid, "malformed fixture")
            .unwrap();
        let transaction = store.begin_transaction().unwrap();
        GraphStore::ensure_contract_derivation_v2_on(&transaction).unwrap();
        store.commit_transaction(&transaction).unwrap();
        drop(transaction);

        assert_eq!(
            store.contract_derivation_failures(None).unwrap(),
            vec![repo.uid.clone()]
        );
        store.delete_repo_node(&repo.uid).unwrap();
        assert!(store.contract_derivation_failures(None).unwrap().is_empty());
    }

    /// `root_path` round-trips through insert → list_repos/lookup_repo:
    /// `Some(path)` survives, `None` stays `None` (stored as '' and mapped
    /// back on read).
    #[test]
    fn repo_root_path_round_trips_some_and_none() {
        let store = test_store();

        let with_root = Repo {
            root_path: Some("/home/u/demo".to_string()),
            url: "https://github.com/acme/demo.git".to_string(),
            ..make_repo("repo-local")
        };
        let without_root = make_repo("repo-remote");
        store.insert_repo(&with_root).unwrap();
        store.insert_repo(&without_root).unwrap();

        let repos = store.list_repos(None).unwrap();
        let local = repos.iter().find(|r| r.uid == "repo-local").unwrap();
        let remote = repos.iter().find(|r| r.uid == "repo-remote").unwrap();
        assert_eq!(local.root_path.as_deref(), Some("/home/u/demo"));
        assert_eq!(remote.root_path, None);

        let looked_up = store.lookup_repo("repo-local").unwrap().unwrap();
        assert_eq!(looked_up.root_path.as_deref(), Some("/home/u/demo"));
        let looked_up = store.lookup_repo("repo-remote").unwrap().unwrap();
        assert_eq!(looked_up.root_path, None);
    }

    /// `update_repo_sha` re-creates the Repo node — it must carry
    /// `root_path` over, not silently drop it.
    #[test]
    fn update_repo_sha_preserves_root_path() {
        let store = test_store();
        let repo = Repo {
            root_path: Some("/home/u/demo".to_string()),
            ..make_repo("repo-1")
        };
        store.insert_repo(&repo).unwrap();

        store.update_repo_sha("repo-1", "def456").unwrap();

        let r = store.lookup_repo("repo-1").unwrap().unwrap();
        assert_eq!(r.indexed_sha, "def456");
        assert_eq!(r.root_path.as_deref(), Some("/home/u/demo"));
    }

    /// `update_repo_root_path` sets only the disk location, leaving the
    /// identity url and every other field untouched.
    #[test]
    fn update_repo_root_path_sets_location_only() {
        let store = test_store();
        store.insert_repo(&make_repo("repo-1")).unwrap();

        store
            .update_repo_root_path("repo-1", "/moved/here")
            .unwrap();

        let r = store.lookup_repo("repo-1").unwrap().unwrap();
        assert_eq!(r.root_path.as_deref(), Some("/moved/here"));
        assert_eq!(r.url, "https://github.com/example/repo-1");
        assert_eq!(r.indexed_sha, "abc123");
    }

    #[test]
    fn insert_and_lookup_symbol_by_uid() {
        let store = test_store();
        let sym = make_symbol("sym-1", "my_func", "repo-1", "src/lib.rs");
        store.insert_symbol(&sym).unwrap();

        let found = store.lookup_symbol("sym-1").unwrap();
        assert_eq!(found.uid, "sym-1");
        assert_eq!(found.name, "my_func");
        assert_eq!(found.kind, SymbolKind::Function);
        assert_eq!(found.repo_uid, "repo-1");
        // P0.1: end_line round-trips independently of start_line.
        assert_eq!(found.start_line, 10);
        assert_eq!(found.end_line, 25);
    }

    #[test]
    fn lookup_symbol_by_name_returns_matches() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("s1", "process", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s2", "process", "repo-1", "b.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s3", "other", "repo-1", "c.rs"))
            .unwrap();

        let found = store.lookup_symbols_by_name("process").unwrap();
        assert_eq!(found.len(), 2);
        let uids: Vec<_> = found.iter().map(|s| s.uid.as_str()).collect();
        assert!(uids.contains(&"s1"));
        assert!(uids.contains(&"s2"));
    }

    #[test]
    fn lookup_missing_symbol_returns_not_found() {
        let store = test_store();
        let err = store.lookup_symbol("nonexistent").unwrap_err();
        assert!(matches!(err, crate::StoreError::NotFound));
    }

    #[test]
    fn insert_and_query_edges() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("caller", "caller_fn", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("callee", "callee_fn", "repo-1", "b.rs"))
            .unwrap();

        let edge = ResolvedEdge {
            source_uid: "caller".to_string(),
            target_uid: "callee".to_string(),
            edge_type: EdgeType::Calls,
            confidence: 0.9,
            link_type: None,
            evidence: Vec::new(),
        };
        store.insert_edge(&edge).unwrap();

        let callees = store.callees_of("caller").unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].uid, "callee");
    }

    #[test]
    fn callers_of_returns_calling_symbols() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("sym-a", "fn_a", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym-b", "fn_b", "repo-1", "b.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym-c", "fn_c", "repo-1", "c.rs"))
            .unwrap();

        // a and b both call c
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym-a".to_string(),
                target_uid: "sym-c".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.8,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym-b".to_string(),
                target_uid: "sym-c".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.7,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let callers = store.callers_of("sym-c").unwrap();
        assert_eq!(callers.len(), 2);
        let uids: Vec<_> = callers.iter().map(|s| s.uid.as_str()).collect();
        assert!(uids.contains(&"sym-a"));
        assert!(uids.contains(&"sym-b"));
    }

    #[test]
    fn list_repos_returns_all() {
        let store = test_store();
        store.insert_repo(&make_repo("r1")).unwrap();
        store.insert_repo(&make_repo("r2")).unwrap();
        store.insert_repo(&make_repo("r3")).unwrap();

        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 3);
    }

    #[test]
    fn batch_insert_symbols_works() {
        let store = test_store();
        let symbols: Vec<Symbol> = (0..5)
            .map(|i| {
                make_symbol(
                    &format!("sym-{i}"),
                    &format!("fn_{i}"),
                    "repo-1",
                    "src/x.rs",
                )
            })
            .collect();

        store.batch_insert_symbols(&symbols).unwrap();

        let found = store.lookup_symbol("sym-3").unwrap();
        assert_eq!(found.name, "fn_3");
    }

    /// nw-122: `broken_wikilinks` must report the link TARGET, not the visible
    /// alias. For `[[Home|workspace]]` it used to report "workspace" — a string
    /// that appears nowhere in the vault, so grepping for the reported link
    /// found nothing.
    ///
    /// Rows written before the `target` column existed have an empty target and
    /// MUST fall back to `display`, which is exactly what they have always held.
    /// Without that fallback an upgrade would blank the text on every
    /// pre-existing broken link.
    #[test]
    fn broken_wikilinks_reports_target_and_falls_back_to_display_pre_migration() {
        let store = GraphStore::in_memory().unwrap();
        let vault = Vault {
            uid: "vlt:t:1".into(),
            name: "t".into(),
            root_path: "/t".into(),
            instance_id: "t".into(),
        };
        store.insert_vault(&vault).unwrap();

        let mk_note = |uid: &str, title: &str| nestweaver_schema::Note {
            uid: uid.to_string(),
            vault_uid: vault.uid.clone(),
            file_path: format!("{title}.md"),
            title: title.to_string(),
            note_kind: nestweaver_schema::NoteKind::General,
            word_count: 1,
            content_hash: String::new(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        let src = mk_note("note:src", "Source");
        let dst = mk_note("note:dst", "Home");
        store.insert_note(&src).unwrap();
        store.insert_note(&dst).unwrap();

        let sec = nestweaver_schema::Section {
            uid: "sec:src:1".into(),
            note_uid: src.uid.clone(),
            heading_uid: None,
            start_line: 1,
            end_line: 2,
            text_hash: String::new(),
            text_content: "x".into(),
            word_count: 1,
            pagerank_score: None,
        };
        store.insert_section(&sec).unwrap();
        store
            .batch_insert_note_section_edges(&[(src.uid.as_str(), sec.uid.as_str())])
            .unwrap();

        // A piped link: visible text "workspace", target "Home". Confidence < 1
        // so it lands in the broken/low-confidence report.
        store
            .batch_insert_wikilink_to_note_edges(&[(
                sec.uid.as_str(),
                dst.uid.as_str(),
                0.9,
                "workspace",
                "Home",
            )])
            .unwrap();

        let rows = store.broken_wikilinks().unwrap();
        let texts: Vec<&str> = rows.iter().map(|r| r.wikilink_text.as_str()).collect();
        assert!(
            texts.contains(&"Home"),
            "must report the link target, got {texts:?}"
        );
        assert!(
            !texts.contains(&"workspace"),
            "must not report the display alias as the link: {texts:?}"
        );
    }

    #[test]
    fn insert_repo_file_and_symbol_edges() {
        let store = test_store();
        let repo = make_repo("repo-e1");
        let file = make_file("file-e1", "repo-e1");
        let sym = make_symbol("sym-e1", "edge_fn", "repo-e1", "src/edge.rs");

        store.insert_repo(&repo).unwrap();
        store.insert_file(&file).unwrap();
        store.insert_symbol(&sym).unwrap();

        store.insert_repo_file_edge("repo-e1", "file-e1").unwrap();
        store.insert_file_symbol_edge("file-e1", "sym-e1").unwrap();
    }

    #[test]
    fn insert_service_works() {
        let store = test_store();
        let svc = make_service("svc-1", "repo-1");
        store.insert_service(&svc).unwrap();

        let services = store.list_services(None).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].uid, "svc-1");
    }

    #[test]
    fn cross_repo_link_edge_works() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("sym-x", "fn_x", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym-y", "fn_y", "repo-2", "b.rs"))
            .unwrap();

        let edge = ResolvedEdge {
            source_uid: "sym-x".to_string(),
            target_uid: "sym-y".to_string(),
            edge_type: EdgeType::CrossRepoLink,
            confidence: 0.6,
            link_type: Some(nestweaver_schema::CrossRepoLinkType::SharedImport),
            evidence: Vec::new(),
        };
        store.insert_edge(&edge).unwrap();
    }

    /// `references_to` backs the ImpactAnalysis RPC / pre-push-impact. It must
    /// follow CROSS_REPO_LINK so that pre-push impact reports cross-repo
    /// consumers, not just same-repo references.
    #[test]
    fn references_to_includes_cross_repo_link() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("api", "ApiHandler", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("client", "RemoteClient", "repo-2", "b.rs"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "client".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(nestweaver_schema::CrossRepoLinkType::SharedImport),
                evidence: Vec::new(),
            })
            .unwrap();

        let refs = store.references_to("api").unwrap();
        assert!(
            refs.iter().any(|s| s.uid == "client"),
            "references_to must include cross-repo consumers via CROSS_REPO_LINK; got: {:?}",
            refs.iter().map(|s| s.uid.as_str()).collect::<Vec<_>>()
        );
    }

    /// `files_referencing_file` (nw-008) returns the 1-hop reverse-dependent
    /// files: every file with a cross-file resolved edge pointing INTO the
    /// target file. Intra-file edges and edges into other files must be ignored.
    #[test]
    fn files_referencing_file_returns_cross_file_dependents() {
        let store = test_store();
        // b.rs imports a.rs (cross-file). c.rs has only an intra-file CALLS edge.
        store
            .insert_symbol(&make_symbol("a1", "exported", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b1", "importer", "repo-1", "b.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("c1", "caller", "repo-1", "c.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("c2", "callee", "repo-1", "c.rs"))
            .unwrap();
        // b.rs -> a.rs : a cross-file IMPORTS edge.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b1".to_string(),
                target_uid: "a1".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        // c.rs -> c.rs : an intra-file CALLS edge (must NOT count as referencing a.rs).
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "c1".to_string(),
                target_uid: "c2".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let refs = store.files_referencing_file("repo-1", "a.rs").unwrap();
        assert!(
            refs.contains("b.rs"),
            "b.rs imports a.rs and must be a reverse-dependent; got: {refs:?}"
        );
        assert!(
            !refs.contains("a.rs"),
            "the file itself must never appear (intra-file edges excluded); got: {refs:?}"
        );
        assert!(
            !refs.contains("c.rs"),
            "c.rs has no edge into a.rs and must be excluded; got: {refs:?}"
        );

        // a.rs's own reverse-deps for a different target are empty.
        let none = store.files_referencing_file("repo-1", "c.rs").unwrap();
        assert!(
            !none.contains("c.rs"),
            "intra-file edge must not make c.rs reference itself; got: {none:?}"
        );
        assert!(
            none.is_empty(),
            "c.rs has no cross-file dependents; got: {none:?}"
        );
    }

    #[test]
    fn count_symbols_by_repo_groups_correctly() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("a", "fa", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b", "fb", "repo-1", "b.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("c", "fc", "repo-2", "c.rs"))
            .unwrap();

        let counts = store.count_symbols_by_repo().unwrap();
        assert_eq!(counts.get("repo-1").copied(), Some(2));
        assert_eq!(counts.get("repo-2").copied(), Some(1));
    }

    /// `callees_of` backs flow_trace's forward traversal. It must follow
    /// CROSS_REPO_LINK so a trace can continue across a repo boundary into the
    /// downstream symbol in another repo.
    #[test]
    fn callees_of_includes_cross_repo_link() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("caller", "LocalCaller", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("remote", "RemoteCallee", "repo-2", "b.rs"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "remote".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(nestweaver_schema::CrossRepoLinkType::SharedImport),
                evidence: Vec::new(),
            })
            .unwrap();

        let callees = store.callees_of("caller").unwrap();
        assert!(
            callees.iter().any(|s| s.uid == "remote"),
            "callees_of must include cross-repo callees via CROSS_REPO_LINK; got: {:?}",
            callees.iter().map(|s| s.uid.as_str()).collect::<Vec<_>>()
        );
    }

    // ── Brain extension round-trip tests ────────────────────────────────

    #[test]
    fn insert_and_list_vault() {
        use nestweaver_schema::Vault;
        let store = test_store();
        let vault = Vault {
            uid: "vlt:test:abc".to_string(),
            name: "my-vault".to_string(),
            root_path: "/tmp/vault".to_string(),
            instance_id: "default".to_string(),
        };
        store.insert_vault(&vault).unwrap();

        let vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        assert_eq!(vaults[0].uid, "vlt:test:abc");
        assert_eq!(vaults[0].name, "my-vault");
    }

    #[test]
    fn insert_and_list_notes() {
        use nestweaver_schema::{Note, NoteKind};
        let store = test_store();
        let note = Note {
            uid: "note:vlt:test:abc:def".to_string(),
            vault_uid: "vlt:test:abc".to_string(),
            file_path: "notes/hello.md".to_string(),
            title: "Hello".to_string(),
            note_kind: NoteKind::General,
            word_count: 42,
            content_hash: "deadbeef".to_string(),
            frontmatter: Some("{}".to_string()),
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.insert_note(&note).unwrap();

        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "Hello");
        assert_eq!(notes[0].word_count, 42);
        assert_eq!(notes[0].note_kind, NoteKind::General);

        let count = store.count_notes().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn list_notes_filtered_by_vault() {
        use nestweaver_schema::{Note, NoteKind};
        let store = test_store();
        for (uid, vault, title) in [
            ("note:a:1", "vlt:a", "A1"),
            ("note:a:2", "vlt:a", "A2"),
            ("note:b:1", "vlt:b", "B1"),
        ] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: vault.to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::General,
                    word_count: 0,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
        }

        let only_a = store.list_notes(Some("vlt:a")).unwrap();
        assert_eq!(only_a.len(), 2);
        assert!(only_a.iter().all(|n| n.vault_uid == "vlt:a"));
    }

    #[test]
    fn lookup_note_by_uid() {
        use nestweaver_schema::{Note, NoteKind};
        let store = test_store();
        store
            .insert_note(&Note {
                uid: "note:test:1".to_string(),
                vault_uid: "vlt:test".to_string(),
                file_path: "x.md".to_string(),
                title: "Lookup Target".to_string(),
                note_kind: NoteKind::Prd,
                word_count: 100,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let found = store.lookup_note("note:test:1").unwrap();
        assert_eq!(found.title, "Lookup Target");
        assert_eq!(found.note_kind, NoteKind::Prd);
    }

    #[test]
    fn insert_and_list_headings_and_sections() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Vault};
        let store = test_store();

        // Vault + Note prerequisites.
        store
            .insert_vault(&Vault {
                uid: "vlt:o".to_string(),
                name: "outline".to_string(),
                root_path: "/o".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:o:1".to_string(),
                vault_uid: "vlt:o".to_string(),
                file_path: "n.md".to_string(),
                title: "n".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let h1 = Heading {
            uid: "head:o:1:1".to_string(),
            note_uid: "note:o:1".to_string(),
            level: 1,
            text: "Top".to_string(),
            slug: "top".to_string(),
            start_line: 1,
            end_line: 1,
            content_hash: "h1".to_string(),
            embedding: None,
        };
        let h2 = Heading {
            uid: "head:o:2:5".to_string(),
            note_uid: "note:o:1".to_string(),
            level: 2,
            text: "Sub".to_string(),
            slug: "sub".to_string(),
            start_line: 5,
            end_line: 5,
            content_hash: "h2".to_string(),
            embedding: None,
        };
        store
            .batch_insert_headings(&[h1.clone(), h2.clone()])
            .unwrap();

        let s1 = Section {
            uid: "sec:o:1:abc".to_string(),
            note_uid: "note:o:1".to_string(),
            heading_uid: Some(h1.uid.clone()),
            start_line: 2,
            end_line: 4,
            text_hash: "th1".to_string(),
            text_content: "body of s1".to_string(),
            word_count: 10,
            pagerank_score: None,
        };
        let s2 = Section {
            uid: "sec:o:5:def".to_string(),
            note_uid: "note:o:1".to_string(),
            heading_uid: Some(h2.uid.clone()),
            start_line: 6,
            end_line: 9,
            text_hash: "th2".to_string(),
            text_content: "body of s2".to_string(),
            word_count: 20,
            pagerank_score: None,
        };
        store
            .batch_insert_sections(&[s1.clone(), s2.clone()])
            .unwrap();

        // Edges.
        store
            .batch_insert_note_heading_edges(&[("note:o:1", &h1.uid), ("note:o:1", &h2.uid)])
            .unwrap();
        store
            .batch_insert_note_section_edges(&[("note:o:1", &s1.uid), ("note:o:1", &s2.uid)])
            .unwrap();
        store
            .batch_insert_heading_section_edges(&[(&h1.uid, &s1.uid), (&h2.uid, &s2.uid)])
            .unwrap();
        store
            .batch_insert_heading_parent_edges(&[(&h2.uid, &h1.uid)])
            .unwrap();

        // Reads.
        let headings = store.headings_in_note("note:o:1").unwrap();
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[1].level, 2);

        let sections = store.sections_in_note("note:o:1").unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].word_count, 10);
        assert_eq!(sections[0].heading_uid.as_deref(), Some(h1.uid.as_str()));

        assert_eq!(store.count_headings().unwrap(), 2);
        assert_eq!(store.count_sections().unwrap(), 2);
    }

    #[test]
    fn vault_has_note_edge() {
        use nestweaver_schema::{Note, NoteKind, Vault};
        let store = test_store();
        store
            .insert_vault(&Vault {
                uid: "vlt:x".to_string(),
                name: "x".to_string(),
                root_path: "/x".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:x:1".to_string(),
                vault_uid: "vlt:x".to_string(),
                file_path: "n.md".to_string(),
                title: "n".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        // Should succeed — both endpoints exist.
        store.insert_vault_note_edge("vlt:x", "note:x:1").unwrap();
    }

    /// nw-172 shipped `expand_tags_with_descendants` with no test at all.
    ///
    /// Two rules, and the second was broken: descendants match through a
    /// REQUIRED `/` separator, and matching is case-insensitive because tag
    /// names are canonicalized to lowercase when indexed. Comparing a raw
    /// request against stored names meant `tags=["Project"]` found nothing
    /// while `brain_tag_graph {"tag":"Project"}` — which lowercases — found
    /// the same note. One rule, two answers.
    #[test]
    fn tag_expansion_matches_descendants_case_insensitively() {
        use nestweaver_schema::{Tag, Vault};
        let store = test_store();
        store
            .insert_vault(&Vault {
                uid: "vlt:t".to_string(),
                name: "t".to_string(),
                root_path: "/t".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        // Stored lowercased, exactly as the markdown indexer canonicalizes.
        for (uid, name) in [
            ("tag:t:project", "project"),
            ("tag:t:project-nw", "project/nestweaver"),
            ("tag:t:projectile", "projectile"),
        ] {
            store
                .insert_tag(&Tag {
                    uid: uid.to_string(),
                    vault_uid: "vlt:t".to_string(),
                    name: name.to_string(),
                })
                .unwrap();
        }

        let expanded = |request: &str| -> Vec<String> {
            let mut out = store
                .expand_tags_with_descendants(&[request.to_string()])
                .unwrap();
            out.sort();
            out
        };

        // The parent carries its subtree, and `projectile` is NOT in it — a
        // prefix match without the separator would silently widen the filter.
        assert_eq!(
            expanded("project"),
            vec!["project".to_string(), "project/nestweaver".to_string()]
        );

        // The case the mismatch broke. `#` stripping and case folding must
        // both apply, and the result must equal the canonical request's.
        for request in ["Project", "PROJECT", "#Project"] {
            assert_eq!(
                expanded(request),
                expanded("project"),
                "{request:?} must resolve exactly like its canonical form"
            );
        }

        // A request that matches nothing still passes through, so the caller's
        // own "no such tag" handling fires instead of an empty expansion.
        assert_eq!(expanded("nope"), vec!["nope".to_string()]);
    }

    #[test]
    fn delete_note_cascade_removes_descendants() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Vault};
        let store = test_store();

        store
            .insert_vault(&Vault {
                uid: "vlt:c".to_string(),
                name: "c".to_string(),
                root_path: "/c".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:c:1".to_string(),
                vault_uid: "vlt:c".to_string(),
                file_path: "a.md".to_string(),
                title: "A".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:c:1:1".to_string(),
                note_uid: "note:c:1".to_string(),
                level: 1,
                text: "Top".to_string(),
                slug: "top".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "h".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:c:1:abc".to_string(),
                note_uid: "note:c:1".to_string(),
                heading_uid: Some("head:c:1:1".to_string()),
                start_line: 2,
                end_line: 4,
                text_hash: "th".to_string(),
                text_content: "body".to_string(),
                word_count: 5,
                pagerank_score: None,
            })
            .unwrap();
        store
            .batch_insert_note_heading_edges(&[("note:c:1", "head:c:1:1")])
            .unwrap();
        store
            .batch_insert_note_section_edges(&[("note:c:1", "sec:c:1:abc")])
            .unwrap();

        // Before: 1 note, 1 heading, 1 section.
        assert_eq!(store.count_notes().unwrap(), 1);
        assert_eq!(store.count_headings().unwrap(), 1);
        assert_eq!(store.count_sections().unwrap(), 1);

        store.delete_note_cascade("note:c:1").unwrap();

        // After: all three gone. Vault untouched.
        assert_eq!(store.count_notes().unwrap(), 0);
        assert_eq!(store.count_headings().unwrap(), 0);
        assert_eq!(store.count_sections().unwrap(), 0);
        assert_eq!(store.list_vaults(None).unwrap().len(), 1);
    }

    #[test]
    fn delete_note_cascade_removes_fragments_from_either_ownership_signal() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Vault};
        let store = test_store();

        store
            .insert_vault(&Vault {
                uid: "vlt:partial-note".to_string(),
                name: "partial-note".to_string(),
                root_path: "/partial-note".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:partial-note".to_string(),
                vault_uid: "vlt:partial-note".to_string(),
                file_path: "partial.md".to_string(),
                title: "Partial".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:unrelated".to_string(),
                vault_uid: "vlt:partial-note".to_string(),
                file_path: "unrelated.md".to_string(),
                title: "Unrelated".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "unrelated-note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:partial-note".to_string(),
                note_uid: "note:partial-note".to_string(),
                level: 1,
                text: "Partial".to_string(),
                slug: "partial".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:edge-owned-note".to_string(),
                note_uid: "note:wrong-owner".to_string(),
                level: 1,
                text: "Edge owned".to_string(),
                slug: "edge-owned".to_string(),
                start_line: 3,
                end_line: 3,
                content_hash: "edge-heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:unrelated".to_string(),
                note_uid: "note:unrelated".to_string(),
                level: 1,
                text: "Unrelated".to_string(),
                slug: "unrelated".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "unrelated-heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:partial-note".to_string(),
                note_uid: "note:partial-note".to_string(),
                heading_uid: Some("head:partial-note".to_string()),
                start_line: 2,
                end_line: 2,
                text_hash: "section-hash".to_string(),
                text_content: "partial body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:edge-owned-note".to_string(),
                note_uid: "note:wrong-owner".to_string(),
                heading_uid: Some("head:edge-owned-note".to_string()),
                start_line: 4,
                end_line: 4,
                text_hash: "edge-section-hash".to_string(),
                text_content: "edge-owned body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:unrelated".to_string(),
                note_uid: "note:unrelated".to_string(),
                heading_uid: Some("head:unrelated".to_string()),
                start_line: 2,
                end_line: 2,
                text_hash: "unrelated-section-hash".to_string(),
                text_content: "unrelated body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .batch_insert_note_heading_edges(&[
                ("note:partial-note", "head:edge-owned-note"),
                ("note:unrelated", "head:unrelated"),
            ])
            .unwrap();
        store
            .batch_insert_note_section_edges(&[
                ("note:partial-note", "sec:edge-owned-note"),
                ("note:unrelated", "sec:unrelated"),
            ])
            .unwrap();

        store.delete_note_cascade("note:partial-note").unwrap();

        assert_eq!(store.count_notes().unwrap(), 1);
        assert_eq!(store.count_headings().unwrap(), 1);
        assert_eq!(store.count_sections().unwrap(), 1);
        assert_eq!(store.list_vaults(None).unwrap().len(), 1);
    }

    #[test]
    fn delete_note_cascade_is_a_noop_for_missing_uid() {
        let store = test_store();
        store.delete_note_cascade("note:does:not:exist").unwrap();
        assert_eq!(store.count_notes().unwrap(), 0);
    }

    #[test]
    fn delete_symbols_in_file_removes_only_target_file() {
        use nestweaver_schema::{File, file_uid};
        let store = test_store();
        let repo = make_repo("repo-del-1");
        store.insert_repo(&repo).unwrap();

        // Two symbols in two different files within the same repo.
        let sym_a = make_symbol("sym-del-a", "fn_a", "repo-del-1", "src/a.rs");
        let sym_b = make_symbol("sym-del-b", "fn_b", "repo-del-1", "src/b.rs");
        store.insert_symbol(&sym_a).unwrap();
        store.insert_symbol(&sym_b).unwrap();

        let file_a = File {
            uid: file_uid("repo-del-1", "src/a.rs"),
            path: "src/a.rs".to_string(),
            repo_uid: "repo-del-1".to_string(),
            content_hash: "h1".to_string(),
        };
        let file_b = File {
            uid: file_uid("repo-del-1", "src/b.rs"),
            path: "src/b.rs".to_string(),
            repo_uid: "repo-del-1".to_string(),
            content_hash: "h2".to_string(),
        };
        store.insert_file(&file_a).unwrap();
        store.insert_file(&file_b).unwrap();
        store
            .insert_file_symbol_edge(&file_a.uid, &sym_a.uid)
            .unwrap();
        store
            .insert_file_symbol_edge(&file_b.uid, &sym_b.uid)
            .unwrap();

        // Delete symbols in file A only.
        let deleted = store
            .delete_symbols_in_file("repo-del-1", "src/a.rs")
            .unwrap();
        // nw-204: the UIDs themselves are the contract now — the epilogue
        // tombstones embeddings by them, so returning the right count with the
        // wrong identities would be silently useless.
        assert_eq!(deleted, vec!["sym-del-a".to_string()]);

        // sym-a should be gone; sym-b should remain.
        let err = store.lookup_symbol("sym-del-a").unwrap_err();
        assert!(matches!(err, crate::StoreError::NotFound));
        let still_there = store.lookup_symbol("sym-del-b").unwrap();
        assert_eq!(still_there.uid, "sym-del-b");
    }

    #[test]
    fn symbols_in_file_in_repo_scopes_by_repo() {
        let store = test_store();
        store.insert_repo(&make_repo("repo-scope-1")).unwrap();
        store.insert_repo(&make_repo("repo-scope-2")).unwrap();

        // Same relative path in two different repos.
        let sym_1 = make_symbol("sym-scope-1", "main_1", "repo-scope-1", "src/main.rs");
        let sym_2 = make_symbol("sym-scope-2", "main_2", "repo-scope-2", "src/main.rs");
        store.insert_symbol(&sym_1).unwrap();
        store.insert_symbol(&sym_2).unwrap();

        // Scoped lookup returns only the requested repo's symbol.
        let scoped = store
            .symbols_in_file_in_repo("src/main.rs", "repo-scope-1")
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].uid, "sym-scope-1");

        // The un-scoped lookup returns both.
        let all = store.symbols_in_file("src/main.rs").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn delete_file_node_removes_file() {
        use nestweaver_schema::{File, file_uid};
        let store = test_store();
        let repo = make_repo("repo-del-2");
        store.insert_repo(&repo).unwrap();

        let fuid = file_uid("repo-del-2", "src/main.rs");
        let file = File {
            uid: fuid.clone(),
            path: "src/main.rs".to_string(),
            repo_uid: "repo-del-2".to_string(),
            content_hash: "h3".to_string(),
        };
        store.insert_file(&file).unwrap();

        let sym = make_symbol("sym-del-2", "main_fn", "repo-del-2", "src/main.rs");
        store.insert_symbol(&sym).unwrap();
        store.insert_repo_file_edge("repo-del-2", &fuid).unwrap();
        store.insert_file_symbol_edge(&fuid, &sym.uid).unwrap();

        // Delete the file node.
        store.delete_file_node(&fuid).unwrap();

        // The FILE_HAS_SYMBOL edge is gone (DETACH DELETE removes it), so the
        // symbol can no longer be reached via that edge — but the Symbol node
        // itself is not deleted (delete_file_node only removes the File node).
        // We verify deletion by attempting to look up any symbols in the file
        // path: symbols still exist, but the file node is gone. The easiest
        // observable: re-inserting a File with the same UID should succeed
        // (no duplicate-key error), which proves the node was removed.
        store
            .insert_file(&File {
                uid: fuid.clone(),
                path: "src/main.rs".to_string(),
                repo_uid: "repo-del-2".to_string(),
                content_hash: "h3-new".to_string(),
            })
            .unwrap();
    }

    #[test]
    fn clear_repo_derived_nodes_enables_idempotent_reindex() {
        use nestweaver_schema::Contract;
        let store = test_store();
        let repo = make_repo("repo-clear-1");
        store.insert_repo(&repo).unwrap();

        // Simulate a first index: a Service node (plain CREATE, no upsert) and
        // a Contract node, both with deterministic repo-derived UIDs.
        let svc = make_service("svc:repo-clear-1:src", "repo-clear-1");
        store.insert_service(&svc).unwrap();
        let contract = Contract {
            uid: "contract:repo-clear-1:get:/x".to_string(),
            kind: "rest-endpoint".to_string(),
            verb: Some("GET".to_string()),
            path: Some("/x".to_string()),
            operation_id: None,
            repo_uid: "repo-clear-1".to_string(),
            source_path: "openapi.yaml".to_string(),
            confidence: 0.9,
        };
        store.insert_contract(&contract).unwrap();
        assert_eq!(store.list_services(None).unwrap().len(), 1);

        // Re-indexing the SAME service UID without clearing first would trip the
        // primary-key uniqueness constraint. clear_repo_derived_nodes must make
        // it idempotent.
        store.clear_repo_derived_nodes("repo-clear-1").unwrap();
        assert_eq!(store.list_services(None).unwrap().len(), 0);

        // Second index pass: re-insert the same Service must now succeed.
        store
            .insert_service(&svc)
            .expect("re-insert after clear must not collide on primary key");
        assert_eq!(store.list_services(None).unwrap().len(), 1);

        // Idempotent for repos with nothing to clear.
        store
            .clear_repo_derived_nodes("repo-does-not-exist")
            .unwrap();
    }

    // ── Upsert idempotency tests ─────────────────────────────────────

    #[test]
    fn upsert_project_is_idempotent() {
        use nestweaver_schema::Project;
        let store = test_store();

        let project = Project {
            uid: "proj:test:1".to_string(),
            name: "TestProject".to_string(),
            summary: Some("first pass".to_string()),
            instance_id: "inst-1".to_string(),
        };

        // First insert.
        store.upsert_project(&project).unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].summary.as_deref(), Some("first pass"));

        // Second upsert with updated summary — should replace, not error.
        let project_v2 = Project {
            uid: "proj:test:1".to_string(),
            name: "TestProject".to_string(),
            summary: Some("second pass".to_string()),
            instance_id: "inst-1".to_string(),
        };
        store.upsert_project(&project_v2).unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].summary.as_deref(), Some("second pass"));
    }

    #[test]
    fn upsert_project_deduplicates_by_name_on_instance_id_change() {
        use nestweaver_schema::Project;
        let store = test_store();

        // Insert a project with instance_id "default".
        let v1 = Project {
            uid: "proj:default:abc123".to_string(),
            name: "MyProject".to_string(),
            summary: Some("original".to_string()),
            instance_id: "default".to_string(),
        };
        store.upsert_project(&v1).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);

        // Re-upsert with a different instance_id (and therefore different UID).
        // This simulates the user changing their instance config.
        let v2 = Project {
            uid: "proj:kkehl-work:def456".to_string(),
            name: "MyProject".to_string(),
            summary: Some("updated".to_string()),
            instance_id: "kkehl-work".to_string(),
        };
        store.upsert_project(&v2).unwrap();

        // Should still have exactly one project, not two.
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].uid, "proj:kkehl-work:def456");
        assert_eq!(projects[0].summary.as_deref(), Some("updated"));
    }

    #[test]
    fn upsert_note_is_idempotent() {
        use nestweaver_schema::{Note, NoteKind, Section};
        let store = test_store();

        let note = Note {
            uid: "note:up:1".to_string(),
            vault_uid: "vlt:up".to_string(),
            file_path: "up.md".to_string(),
            title: "Upsert".to_string(),
            note_kind: NoteKind::General,
            word_count: 10,
            content_hash: "h1".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };

        // First insert.
        store.upsert_note(&note).unwrap();
        assert_eq!(store.count_notes().unwrap(), 1);

        // Add a child section so the cascade has something to clean up.
        store
            .insert_section(&Section {
                uid: "sec:up:1:abc".to_string(),
                note_uid: "note:up:1".to_string(),
                heading_uid: None,
                start_line: 1,
                end_line: 3,
                text_hash: "th1".to_string(),
                text_content: "hello".to_string(),
                word_count: 1,
                pagerank_score: None,
            })
            .unwrap();
        store
            .batch_insert_note_section_edges(&[("note:up:1", "sec:up:1:abc")])
            .unwrap();
        assert_eq!(store.count_sections().unwrap(), 1);

        // Second upsert — should cascade-delete the old note + section, then
        // re-insert the note. No duplicate-PK error.
        let note_v2 = Note {
            uid: "note:up:1".to_string(),
            vault_uid: "vlt:up".to_string(),
            file_path: "up.md".to_string(),
            title: "Upsert v2".to_string(),
            note_kind: NoteKind::General,
            word_count: 20,
            content_hash: "h2".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.upsert_note(&note_v2).unwrap();

        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "Upsert v2");
        assert_eq!(notes[0].word_count, 20);
        // Old section was cascade-deleted.
        assert_eq!(store.count_sections().unwrap(), 0);
    }

    #[test]
    fn batch_upsert_sections_is_idempotent() {
        use nestweaver_schema::{Note, NoteKind, Section};
        let store = test_store();

        store
            .insert_note(&Note {
                uid: "note:bus:1".to_string(),
                vault_uid: "vlt:bus".to_string(),
                file_path: "bus.md".to_string(),
                title: "Bus".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let sections = vec![Section {
            uid: "sec:bus:1:xyz".to_string(),
            note_uid: "note:bus:1".to_string(),
            heading_uid: None,
            start_line: 1,
            end_line: 5,
            text_hash: "t1".to_string(),
            text_content: "first".to_string(),
            word_count: 1,
            pagerank_score: None,
        }];

        store.batch_upsert_sections(&sections).unwrap();
        assert_eq!(store.count_sections().unwrap(), 1);

        // Upsert again with different content.
        let sections_v2 = vec![Section {
            uid: "sec:bus:1:xyz".to_string(),
            note_uid: "note:bus:1".to_string(),
            heading_uid: None,
            start_line: 1,
            end_line: 5,
            text_hash: "t2".to_string(),
            text_content: "second".to_string(),
            word_count: 1,
            pagerank_score: None,
        }];

        store.batch_upsert_sections(&sections_v2).unwrap();
        assert_eq!(store.count_sections().unwrap(), 1);

        let secs = store.sections_in_note("note:bus:1").unwrap();
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].text_content, "second");
    }

    #[test]
    fn batch_insert_edges_works() {
        let store = test_store();
        for i in 0..3u32 {
            store
                .insert_symbol(&make_symbol(
                    &format!("be-sym-{i}"),
                    &format!("fn_{i}"),
                    "repo-1",
                    "x.rs",
                ))
                .unwrap();
        }
        let edges: Vec<ResolvedEdge> = vec![
            ResolvedEdge {
                source_uid: "be-sym-0".to_string(),
                target_uid: "be-sym-1".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            },
            ResolvedEdge {
                source_uid: "be-sym-1".to_string(),
                target_uid: "be-sym-2".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.8,
                link_type: None,
                evidence: Vec::new(),
            },
        ];
        store.batch_insert_edges(&edges).unwrap();

        let callees = store.callees_of("be-sym-0").unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].uid, "be-sym-1");
    }

    #[test]
    fn contract_round_trips_and_implements_edge() {
        use nestweaver_schema::Contract;
        let store = test_store();

        let contract = Contract {
            uid: "contract:http:POST:/v1/approvals".to_string(),
            kind: "http".to_string(),
            verb: Some("POST".to_string()),
            path: Some("/v1/approvals".to_string()),
            operation_id: Some("createApproval".to_string()),
            repo_uid: "repo-1".to_string(),
            source_path: "openapi.yaml".to_string(),
            confidence: 1.0,
        };
        store.insert_contract(&contract).unwrap();

        // Read back (no filter, and repo-filtered).
        let all = store.list_contracts(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].uid, "contract:http:POST:/v1/approvals");
        assert_eq!(all[0].verb.as_deref(), Some("POST"));
        assert_eq!(all[0].path.as_deref(), Some("/v1/approvals"));
        assert_eq!(all[0].confidence, 1.0);
        assert!(store.list_contracts(Some("repo-2")).unwrap().is_empty());

        // Insert is idempotent (no duplicate after re-insert).
        store.insert_contract(&contract).unwrap();
        assert_eq!(store.list_contracts(None).unwrap().len(), 1);

        // A handler symbol implements it.
        store
            .insert_symbol(&make_symbol(
                "h1",
                "create",
                "repo-1",
                "ApprovalsController.java",
            ))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "h1".to_string(),
                target_uid: "contract:http:POST:/v1/approvals".to_string(),
                edge_type: EdgeType::ImplementsContract,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let implemented = store.list_implemented_contract_uids().unwrap();
        assert_eq!(implemented, vec!["contract:http:POST:/v1/approvals"]);
    }

    #[test]
    fn test_transactional_note_insert() {
        use nestweaver_schema::{Note, NoteKind};
        let store = test_store();

        let notes = vec![
            Note {
                uid: "note:txn:1".to_string(),
                vault_uid: "vlt:txn".to_string(),
                file_path: "txn/a.md".to_string(),
                title: "Txn Note A".to_string(),
                note_kind: NoteKind::General,
                word_count: 10,
                content_hash: "aaa".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
            Note {
                uid: "note:txn:2".to_string(),
                vault_uid: "vlt:txn".to_string(),
                file_path: "txn/b.md".to_string(),
                title: "Txn Note B".to_string(),
                note_kind: NoteKind::General,
                word_count: 20,
                content_hash: "bbb".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
        ];

        let conn = store.begin_transaction().unwrap();
        GraphStore::batch_insert_notes_on(&conn, &notes).unwrap();
        store.commit_transaction(&conn).unwrap();

        let count = store.count_notes().unwrap();
        assert_eq!(count, 2);
        let listed = store.list_notes(Some("vlt:txn")).unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn bulk_vault_write_inserts_notes_headings_sections_and_edges() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Tag, Vault};
        let store = test_store();

        // Insert the vault first so VAULT_HAS_NOTE edge MATCH finds it.
        let vault = Vault {
            uid: "vlt:bvw".to_string(),
            name: "bvw-vault".to_string(),
            root_path: "/tmp/bvw".to_string(),
            instance_id: "default".to_string(),
        };
        store.insert_vault(&vault).unwrap();

        let notes = vec![
            Note {
                uid: "note:bvw:n1".to_string(),
                vault_uid: "vlt:bvw".to_string(),
                file_path: "n1.md".to_string(),
                title: "Note One".to_string(),
                note_kind: NoteKind::General,
                word_count: 10,
                content_hash: "h1".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
            Note {
                uid: "note:bvw:n2".to_string(),
                vault_uid: "vlt:bvw".to_string(),
                file_path: "n2.md".to_string(),
                title: "Note Two".to_string(),
                note_kind: NoteKind::General,
                word_count: 20,
                content_hash: "h2".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
        ];

        let headings = vec![Heading {
            uid: "hdg:bvw:h1".to_string(),
            note_uid: "note:bvw:n1".to_string(),
            level: 2,
            text: "Intro".to_string(),
            slug: "intro".to_string(),
            start_line: 1,
            end_line: 5,
            content_hash: "hh1".to_string(),
            embedding: None,
        }];

        let sections = vec![Section {
            uid: "sec:bvw:s1".to_string(),
            note_uid: "note:bvw:n1".to_string(),
            heading_uid: Some("hdg:bvw:h1".to_string()),
            start_line: 1,
            end_line: 5,
            text_hash: "sh1".to_string(),
            text_content: "Section body text.".to_string(),
            word_count: 3,
            pagerank_score: None,
        }];

        let tags = vec![Tag {
            uid: "tag:bvw:rust".to_string(),
            vault_uid: "vlt:bvw".to_string(),
            name: "rust".to_string(),
        }];

        let vault_note_edges: Vec<(&str, &str)> =
            vec![("vlt:bvw", "note:bvw:n1"), ("vlt:bvw", "note:bvw:n2")];
        let note_heading_edges: Vec<(&str, &str)> = vec![("note:bvw:n1", "hdg:bvw:h1")];
        let note_section_edges: Vec<(&str, &str)> = vec![("note:bvw:n1", "sec:bvw:s1")];
        let heading_section_edges: Vec<(&str, &str)> = vec![("hdg:bvw:h1", "sec:bvw:s1")];
        let note_tag_edges: Vec<(&str, &str)> = vec![("note:bvw:n1", "tag:bvw:rust")];
        let wikilink_to_note_edges: Vec<(&str, &str, f32, &str, &str)> =
            vec![("sec:bvw:s1", "note:bvw:n2", 1.0, "Note Two", "Note Two")];

        store
            .bulk_vault_write(
                &notes,
                &headings,
                &sections,
                &vault_note_edges,
                &note_heading_edges,
                &note_section_edges,
                &heading_section_edges,
                &[],
                &tags,
                &note_tag_edges,
                &[],
                &wikilink_to_note_edges,
                &[],
            )
            .unwrap();

        // Verify nodes were inserted.
        let count = store.count_notes().unwrap();
        assert_eq!(count, 2);

        let listed = store.list_notes(Some("vlt:bvw")).unwrap();
        assert_eq!(listed.len(), 2);
        let titles: Vec<_> = listed.iter().map(|n| n.title.as_str()).collect();
        assert!(titles.contains(&"Note One"));
        assert!(titles.contains(&"Note Two"));
    }

    #[test]
    fn batch_lookup_symbols_returns_all_found() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("sym:batch-1", "alpha", "repo-1", "a.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym:batch-2", "beta", "repo-1", "b.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym:batch-3", "gamma", "repo-1", "c.rs"))
            .unwrap();

        let map = store
            .batch_lookup_symbols(&["sym:batch-1", "sym:batch-2", "sym:batch-3"])
            .unwrap();

        assert_eq!(map.len(), 3);
        assert_eq!(map["sym:batch-1"].name, "alpha");
        assert_eq!(map["sym:batch-2"].name, "beta");
        assert_eq!(map["sym:batch-3"].name, "gamma");
    }

    #[test]
    fn batch_lookup_symbols_missing_uids_absent_from_map() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol("sym:present", "present_fn", "repo-1", "a.rs"))
            .unwrap();

        let map = store
            .batch_lookup_symbols(&["sym:present", "sym:ghost"])
            .unwrap();

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("sym:present"));
        assert!(!map.contains_key("sym:ghost"));
    }

    #[test]
    fn exact_batch_lookup_rejects_a_missing_uid() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol(
                "sym:exact-present",
                "present_fn",
                "repo-1",
                "a.rs",
            ))
            .unwrap();

        let error = store
            .batch_lookup_symbols_exact(&["sym:exact-present", "sym:exact-ghost"])
            .expect_err("an exact lookup must not silently omit a requested symbol");

        assert!(
            error
                .to_string()
                .contains("missing symbol UID sym:exact-ghost"),
            "the error must identify the missing primary key, got: {error}"
        );
    }

    #[test]
    fn exact_batch_lookup_rejects_duplicate_requested_uids() {
        let store = test_store();
        store
            .insert_symbol(&make_symbol(
                "sym:exact-duplicate",
                "duplicate_fn",
                "repo-1",
                "a.rs",
            ))
            .unwrap();

        let error = store
            .batch_lookup_symbols_exact(&["sym:exact-duplicate", "sym:exact-duplicate"])
            .expect_err("an exact lookup must reject duplicate requested primary keys");

        assert!(
            error.to_string().contains("duplicate requested UID"),
            "the error must identify the duplicate request, got: {error}"
        );
    }

    #[test]
    fn batch_lookup_symbols_empty_input_returns_empty_map() {
        let store = test_store();
        let map = store.batch_lookup_symbols(&[]).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_delete_vault_cascade_bulk_removes_all_node_types() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Tag, Vault};
        let store = test_store();

        let vault = Vault {
            uid: "vlt:cas".to_string(),
            name: "cascade-vault".to_string(),
            root_path: "/tmp/cas".to_string(),
            instance_id: "default".to_string(),
        };
        store.insert_vault(&vault).unwrap();

        let make_note = |uid: &str, n: u32| Note {
            uid: uid.to_string(),
            vault_uid: "vlt:cas".to_string(),
            file_path: format!("n{n}.md"),
            title: format!("Note {n}"),
            note_kind: NoteKind::General,
            word_count: n,
            content_hash: format!("h{n}"),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };

        let notes = vec![
            make_note("note:cas:1", 1),
            make_note("note:cas:2", 2),
            make_note("note:cas:3", 3),
        ];
        store.batch_insert_notes(&notes).unwrap();
        store
            .batch_insert_vault_note_edges(&[
                ("vlt:cas", "note:cas:1"),
                ("vlt:cas", "note:cas:2"),
                ("vlt:cas", "note:cas:3"),
            ])
            .unwrap();

        let headings = vec![
            Heading {
                uid: "hdg:cas:1".to_string(),
                note_uid: "note:cas:1".to_string(),
                level: 1,
                text: "H1".to_string(),
                slug: "h1".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "hh1".to_string(),
                embedding: None,
            },
            Heading {
                uid: "hdg:cas:2".to_string(),
                note_uid: "note:cas:2".to_string(),
                level: 1,
                text: "H2".to_string(),
                slug: "h2".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "hh2".to_string(),
                embedding: None,
            },
        ];
        store.batch_insert_headings(&headings).unwrap();

        let sections = vec![
            Section {
                uid: "sec:cas:1".to_string(),
                note_uid: "note:cas:1".to_string(),
                heading_uid: Some("hdg:cas:1".to_string()),
                start_line: 2,
                end_line: 5,
                text_hash: "th1".to_string(),
                text_content: "body 1".to_string(),
                word_count: 2,
                pagerank_score: None,
            },
            Section {
                uid: "sec:cas:2".to_string(),
                note_uid: "note:cas:2".to_string(),
                heading_uid: Some("hdg:cas:2".to_string()),
                start_line: 2,
                end_line: 5,
                text_hash: "th2".to_string(),
                text_content: "body 2".to_string(),
                word_count: 3,
                pagerank_score: None,
            },
        ];
        store.batch_insert_sections(&sections).unwrap();

        store
            .batch_insert_note_heading_edges(&[
                ("note:cas:1", "hdg:cas:1"),
                ("note:cas:2", "hdg:cas:2"),
            ])
            .unwrap();
        store
            .batch_insert_note_section_edges(&[
                ("note:cas:1", "sec:cas:1"),
                ("note:cas:2", "sec:cas:2"),
            ])
            .unwrap();
        store
            .batch_insert_heading_section_edges(&[
                ("hdg:cas:1", "sec:cas:1"),
                ("hdg:cas:2", "sec:cas:2"),
            ])
            .unwrap();

        let tags = vec![
            Tag {
                uid: "tag:cas:alpha".to_string(),
                vault_uid: "vlt:cas".to_string(),
                name: "alpha".to_string(),
            },
            Tag {
                uid: "tag:cas:beta".to_string(),
                vault_uid: "vlt:cas".to_string(),
                name: "beta".to_string(),
            },
        ];
        store.batch_insert_tags(&tags).unwrap();
        store
            .batch_insert_note_tag_edges(&[
                ("note:cas:1", "tag:cas:alpha"),
                ("note:cas:3", "tag:cas:beta"),
            ])
            .unwrap();

        // Confirm pre-delete counts.
        assert!(store.count_notes().unwrap() > 0);
        assert!(store.count_headings().unwrap() > 0);
        assert!(store.count_sections().unwrap() > 0);
        assert!(store.count_tags().unwrap() > 0);

        let deleted = store.delete_vault_cascade("vlt:cas").unwrap();

        assert_eq!(deleted, 3);
        assert_eq!(store.count_notes().unwrap(), 0);
        assert_eq!(store.count_headings().unwrap(), 0);
        assert_eq!(store.count_sections().unwrap(), 0);
        assert_eq!(store.count_tags().unwrap(), 0);
        // Vault node itself should be gone — list_vaults returns nothing.
        assert_eq!(store.list_vaults(None).unwrap().len(), 0);
    }

    #[test]
    fn delete_vault_cascade_removes_fragments_from_either_ownership_signal() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Vault};
        let store = test_store();

        store
            .insert_vault(&Vault {
                uid: "vlt:partial-vault".to_string(),
                name: "partial-vault".to_string(),
                root_path: "/partial-vault".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:unrelated-vault".to_string(),
                name: "unrelated-vault".to_string(),
                root_path: "/unrelated-vault".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:partial-vault".to_string(),
                vault_uid: "vlt:partial-vault".to_string(),
                file_path: "partial.md".to_string(),
                title: "Partial".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:unrelated-vault".to_string(),
                vault_uid: "vlt:unrelated-vault".to_string(),
                file_path: "unrelated.md".to_string(),
                title: "Unrelated".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "unrelated-note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:partial-vault".to_string(),
                note_uid: "note:partial-vault".to_string(),
                level: 1,
                text: "Partial".to_string(),
                slug: "partial".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:edge-owned-vault".to_string(),
                note_uid: "note:wrong-owner".to_string(),
                level: 1,
                text: "Edge owned".to_string(),
                slug: "edge-owned".to_string(),
                start_line: 3,
                end_line: 3,
                content_hash: "edge-heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:unrelated-vault".to_string(),
                note_uid: "note:unrelated-vault".to_string(),
                level: 1,
                text: "Unrelated".to_string(),
                slug: "unrelated".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "unrelated-heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:partial-vault".to_string(),
                note_uid: "note:partial-vault".to_string(),
                heading_uid: Some("head:partial-vault".to_string()),
                start_line: 2,
                end_line: 2,
                text_hash: "section-hash".to_string(),
                text_content: "partial body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:edge-owned-vault".to_string(),
                note_uid: "note:wrong-owner".to_string(),
                heading_uid: Some("head:edge-owned-vault".to_string()),
                start_line: 4,
                end_line: 4,
                text_hash: "edge-section-hash".to_string(),
                text_content: "edge-owned body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:unrelated-vault".to_string(),
                note_uid: "note:unrelated-vault".to_string(),
                heading_uid: Some("head:unrelated-vault".to_string()),
                start_line: 2,
                end_line: 2,
                text_hash: "unrelated-section-hash".to_string(),
                text_content: "unrelated body".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();
        store
            .batch_insert_note_heading_edges(&[
                ("note:partial-vault", "head:edge-owned-vault"),
                ("note:unrelated-vault", "head:unrelated-vault"),
            ])
            .unwrap();
        store
            .batch_insert_note_section_edges(&[
                ("note:partial-vault", "sec:edge-owned-vault"),
                ("note:unrelated-vault", "sec:unrelated-vault"),
            ])
            .unwrap();

        let deleted = store.delete_vault_cascade("vlt:partial-vault").unwrap();

        assert_eq!(deleted, 1);
        assert_eq!(store.count_notes().unwrap(), 1);
        assert_eq!(store.count_headings().unwrap(), 1);
        assert_eq!(store.count_sections().unwrap(), 1);
        let vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        assert_eq!(vaults[0].uid, "vlt:unrelated-vault");
    }

    #[test]
    fn test_bulk_delete_repo_files_and_symbols_removes_all() {
        use nestweaver_schema::file_uid;
        let store = test_store();

        let repo = make_repo("repo-bulk-del");
        store.insert_repo(&repo).unwrap();

        let files: Vec<File> = (1..=3u32)
            .map(|i| File {
                uid: file_uid("repo-bulk-del", &format!("src/f{i}.rs")),
                path: format!("src/f{i}.rs"),
                repo_uid: "repo-bulk-del".to_string(),
                content_hash: format!("fhash{i}"),
            })
            .collect();
        store.batch_insert_files(&files).unwrap();

        let repo_file_edges: Vec<(&str, &str)> = files
            .iter()
            .map(|f| ("repo-bulk-del", f.uid.as_str()))
            .collect();
        store
            .batch_insert_repo_file_edges(&repo_file_edges)
            .unwrap();

        let symbols: Vec<Symbol> = (1..=5u32)
            .map(|i| {
                make_symbol(
                    &format!("sym:bulk-del:{i}"),
                    &format!("fn_{i}"),
                    "repo-bulk-del",
                    &format!("src/f{}.rs", ((i - 1) % 3) + 1),
                )
            })
            .collect();
        store.batch_insert_symbols(&symbols).unwrap();

        let file_sym_edges: Vec<(&str, &str)> = symbols
            .iter()
            .map(|s| {
                let file_path = s.file_path.as_str();
                let fuid = files
                    .iter()
                    .find(|f| f.path == file_path)
                    .map(|f| f.uid.as_str())
                    .unwrap_or("");
                (fuid, s.uid.as_str())
            })
            .collect();
        store
            .batch_insert_file_symbol_edges(&file_sym_edges)
            .unwrap();

        assert!(store.count_symbols().unwrap() > 0);

        let (file_count, sym_count) = store
            .bulk_delete_repo_files_and_symbols("repo-bulk-del")
            .unwrap();

        assert_eq!(file_count, 3);
        assert_eq!(sym_count, 5);
        assert_eq!(store.count_symbols().unwrap(), 0);

        // No files remain for this repo (list_repos still returns the repo node,
        // but files are gone — verify by checking a lookup would find no symbols).
        let all_syms = store.lookup_symbols_by_name("fn_1").unwrap();
        assert!(all_syms.is_empty());
    }

    #[test]
    fn remove_repo_cascade_deletes_all_data() {
        let store = test_store();
        let repo = make_repo("repo:test:r1");
        let file = make_file("file:test:f1", "repo:test:r1");
        let sym = make_symbol("sym:test:s1", "greet", "repo:test:r1", "src/lib.rs");

        store.insert_repo(&repo).unwrap();
        store.insert_file(&file).unwrap();
        store
            .insert_repo_file_edge("repo:test:r1", "file:test:f1")
            .unwrap();
        store.insert_symbol(&sym).unwrap();
        store
            .insert_file_symbol_edge("file:test:f1", "sym:test:s1")
            .unwrap();

        // Verify data exists before deletion.
        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 1);

        // Delete files/symbols, then derived nodes, then the repo node itself.
        let (file_count, sym_count) = store
            .bulk_delete_repo_files_and_symbols("repo:test:r1")
            .unwrap();
        store.clear_repo_derived_nodes("repo:test:r1").unwrap();
        store.delete_repo_node("repo:test:r1").unwrap();

        assert_eq!(file_count, 1);
        assert_eq!(sym_count, 1);

        // Verify everything is gone.
        let repos = store.list_repos(None).unwrap();
        assert!(repos.is_empty());
        let syms = store.lookup_symbols_by_repo("repo:test:r1").unwrap();
        assert!(syms.is_empty());
    }

    #[test]
    fn purge_instance_cascade_deletes_repos_and_children() {
        let store = test_store();

        // Two repos under instance "ghost" — both should disappear.
        let ghost_repo_a = Repo {
            uid: "repo:ghost:aaaa".to_string(),
            url: "file:///ghost/a".to_string(),
            indexed_sha: "local".to_string(),
            staleness_commits_behind: 0,
            instance_id: "ghost".to_string(),
            name: Some("ghost-a".to_string()),
            root_path: None,
        };
        let ghost_repo_b = Repo {
            uid: "repo:ghost:bbbb".to_string(),
            url: "file:///ghost/b".to_string(),
            indexed_sha: "local".to_string(),
            staleness_commits_behind: 0,
            instance_id: "ghost".to_string(),
            name: Some("ghost-b".to_string()),
            root_path: None,
        };
        store.insert_repo(&ghost_repo_a).unwrap();
        store.insert_repo(&ghost_repo_b).unwrap();

        // One repo under a different instance — must survive intact.
        let keep_repo = Repo {
            uid: "repo:keep:cccc".to_string(),
            url: "file:///keep/c".to_string(),
            indexed_sha: "local".to_string(),
            staleness_commits_behind: 0,
            instance_id: "keep".to_string(),
            name: Some("keep-c".to_string()),
            root_path: None,
        };
        store.insert_repo(&keep_repo).unwrap();

        // Children for each repo.
        store
            .insert_file(&make_file("file-ghost-a", "repo:ghost:aaaa"))
            .unwrap();
        store
            .insert_symbol(&make_symbol(
                "sym-ghost-a",
                "ghost_fn",
                "repo:ghost:aaaa",
                "src/file-ghost-a.rs",
            ))
            .unwrap();
        store
            .insert_service(&make_service("svc-ghost-a", "repo:ghost:aaaa"))
            .unwrap();
        store
            .insert_file(&make_file("file-keep", "repo:keep:cccc"))
            .unwrap();
        store
            .insert_symbol(&make_symbol(
                "sym-keep",
                "keep_fn",
                "repo:keep:cccc",
                "src/file-keep.rs",
            ))
            .unwrap();

        // Orphan rows: simulate a partial `instance merge` that already
        // dropped a Repo node but left its Symbol/File children behind
        // with the source instance still encoded in their UID prefix.
        // `purge_instance` must catch these via the orphan-sweep path
        // even though no `repo:ghost:zzzz` exists to walk down from.
        store
            .insert_file(&make_file("file:repo:ghost:zzzz:orphan", "repo:ghost:zzzz"))
            .unwrap();
        store
            .insert_symbol(&make_symbol(
                "sym:repo:ghost:zzzz:orphan",
                "orphan_fn",
                "repo:ghost:zzzz",
                "src/orphan.rs",
            ))
            .unwrap();
        store
            .insert_service(&make_service(
                "svc:repo:ghost:zzzz:orphan",
                "repo:ghost:zzzz",
            ))
            .unwrap();

        let result = store.purge_instance("ghost").unwrap();
        assert_eq!(result.repos, 2);
        assert_eq!(result.files, 1);
        assert_eq!(result.symbols, 1);
        // 1 orphan File + 1 orphan Symbol + 1 orphan Service.
        assert!(
            result.orphans_swept >= 3,
            "expected at least 3 orphan rows swept, got {}",
            result.orphans_swept
        );

        // Ghost repos are gone.
        let remaining = store.list_repos(None).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uid, "repo:keep:cccc");

        // Orphan symbols are also gone.
        let orphan_syms = store.lookup_symbols_by_name("orphan_fn").unwrap();
        assert!(orphan_syms.is_empty());

        // Keep-instance children are intact.
        let keep_syms = store.lookup_symbols_by_name("keep_fn").unwrap();
        assert_eq!(keep_syms.len(), 1);

        // Re-running on a clean instance is a no-op.
        let again = store.purge_instance("ghost").unwrap();
        assert_eq!(again.repos, 0);
        assert_eq!(again.files, 0);
        assert_eq!(again.symbols, 0);
        assert_eq!(again.orphans_swept, 0);
    }

    #[test]
    fn purge_instance_reports_orphan_only_code_deletions() {
        let store = test_store();
        let missing_repo = "repo:ghost:orphan";
        store
            .insert_file(&make_file("file:repo:ghost:orphan:main", missing_repo))
            .unwrap();
        store
            .insert_symbol(&make_symbol(
                "sym:repo:ghost:orphan:handler",
                "orphan_handler",
                missing_repo,
                "src/main.rs",
            ))
            .unwrap();
        store
            .insert_service(&make_service("svc:repo:ghost:orphan:api", missing_repo))
            .unwrap();

        let result = store.purge_instance("ghost").unwrap();
        assert_eq!(result.repos, 0, "precondition: no top-level repo existed");
        assert_eq!(result.code_orphans_swept, 3);
        assert_eq!(result.orphans_swept, 3);
    }

    #[test]
    fn reparent_vault_preserves_notes_and_children() {
        use nestweaver_schema::{Heading, Note, NoteKind, Section, Tag, Vault};
        let store = test_store();

        // Create a vault with 2 notes, 1 heading, 1 section, and 1 tag.
        let vault = Vault {
            uid: "vlt:old".to_string(),
            name: "my-vault".to_string(),
            root_path: "/tmp/vault".to_string(),
            instance_id: "inst-old".to_string(),
        };
        store.insert_vault(&vault).unwrap();

        let notes = vec![
            Note {
                uid: "note:rp:1".to_string(),
                vault_uid: "vlt:old".to_string(),
                file_path: "a.md".to_string(),
                title: "Note A".to_string(),
                note_kind: NoteKind::General,
                word_count: 10,
                content_hash: "ha".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
            Note {
                uid: "note:rp:2".to_string(),
                vault_uid: "vlt:old".to_string(),
                file_path: "b.md".to_string(),
                title: "Note B".to_string(),
                note_kind: NoteKind::General,
                word_count: 20,
                content_hash: "hb".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            },
        ];
        store.batch_insert_notes(&notes).unwrap();
        store
            .batch_insert_vault_note_edges(&[("vlt:old", "note:rp:1"), ("vlt:old", "note:rp:2")])
            .unwrap();

        let heading = Heading {
            uid: "hdg:rp:1".to_string(),
            note_uid: "note:rp:1".to_string(),
            level: 1,
            text: "Heading 1".to_string(),
            slug: "heading-1".to_string(),
            start_line: 1,
            end_line: 1,
            content_hash: "hh1".to_string(),
            embedding: None,
        };
        store.insert_heading(&heading).unwrap();
        store
            .batch_insert_note_heading_edges(&[("note:rp:1", "hdg:rp:1")])
            .unwrap();

        let section = Section {
            uid: "sec:rp:1".to_string(),
            note_uid: "note:rp:1".to_string(),
            heading_uid: Some("hdg:rp:1".to_string()),
            start_line: 2,
            end_line: 5,
            text_hash: "th1".to_string(),
            text_content: "body text".to_string(),
            word_count: 2,
            pagerank_score: None,
        };
        store.insert_section(&section).unwrap();
        store
            .batch_insert_note_section_edges(&[("note:rp:1", "sec:rp:1")])
            .unwrap();
        store
            .batch_insert_heading_section_edges(&[("hdg:rp:1", "sec:rp:1")])
            .unwrap();

        let tag = Tag {
            uid: "tag:rp:alpha".to_string(),
            vault_uid: "vlt:old".to_string(),
            name: "alpha".to_string(),
        };
        store.insert_tag(&tag).unwrap();
        store
            .batch_insert_note_tag_edges(&[("note:rp:1", "tag:rp:alpha")])
            .unwrap();
        store
            .batch_insert_section_tag_edges(&[("sec:rp:1", "tag:rp:alpha")])
            .unwrap();

        // Reparent to new vault.
        let result = store
            .reparent_vault("vlt:old", "vlt:new", "inst-new")
            .unwrap();

        assert_eq!(result.notes_migrated, 2);
        assert_eq!(result.headings_migrated, 1);
        assert_eq!(result.sections_migrated, 1);
        assert_eq!(result.tags_migrated, 1);

        // Old vault is gone.
        let vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        assert_eq!(vaults[0].uid, "vlt:new");
        assert_eq!(vaults[0].instance_id, "inst-new");
        assert_eq!(vaults[0].root_path, "/tmp/vault");

        // Both notes survived under the new vault_uid.
        let new_notes = store.list_notes(Some("vlt:new")).unwrap();
        assert_eq!(new_notes.len(), 2);
        for n in &new_notes {
            assert_eq!(n.vault_uid, "vlt:new");
        }

        // Heading survived.
        let hdgs = store.list_headings_by_vault("vlt:new").unwrap();
        assert_eq!(hdgs.len(), 1);
        assert_eq!(hdgs[0].text, "Heading 1");

        // Section survived.
        let secs = store.list_sections_by_vault("vlt:new").unwrap();
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].text_content, "body text");

        // Tag survived under new vault_uid.
        let tags = store.list_tags(Some("vlt:new")).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "alpha");
        assert_eq!(tags[0].vault_uid, "vlt:new");

        // SECTION_TAGGED_WITH edge survived reparent.
        let section_uids_with_alpha = store
            .list_section_uids_with_tags(&["alpha".to_string()])
            .unwrap();
        assert!(
            section_uids_with_alpha.contains("sec:rp:1"),
            "SECTION_TAGGED_WITH edge was not preserved across reparent"
        );

        // Old vault has no leftovers.
        assert_eq!(store.list_notes(Some("vlt:old")).unwrap().len(), 0);
        assert_eq!(store.list_tags(Some("vlt:old")).unwrap().len(), 0);
    }

    #[test]
    fn merge_instance_ids_preserves_notes() {
        use nestweaver_schema::uid::vault_uid;
        use nestweaver_schema::{Note, NoteKind, Vault};
        let store = test_store();

        // Create a source vault with 3 notes.
        let src_vault = Vault {
            uid: "vlt:src:v1".to_string(),
            name: "my-vault".to_string(),
            root_path: "/tmp/vault".to_string(),
            instance_id: "src".to_string(),
        };
        store.insert_vault(&src_vault).unwrap();

        let make_note = |uid: &str, n: u32| Note {
            uid: uid.to_string(),
            vault_uid: "vlt:src:v1".to_string(),
            file_path: format!("n{n}.md"),
            title: format!("Note {n}"),
            note_kind: NoteKind::General,
            word_count: n,
            content_hash: format!("h{n}"),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        let notes = vec![
            make_note("note:m:1", 1),
            make_note("note:m:2", 2),
            make_note("note:m:3", 3),
        ];
        store.batch_insert_notes(&notes).unwrap();
        store
            .batch_insert_vault_note_edges(&[
                ("vlt:src:v1", "note:m:1"),
                ("vlt:src:v1", "note:m:2"),
                ("vlt:src:v1", "note:m:3"),
            ])
            .unwrap();

        // Merge src -> tgt (no collision — no target vault exists).
        let result = store.merge_instance_ids("src", "tgt").unwrap();
        assert_eq!(result.vaults, 1);
        // No notes should be discarded — they should survive via reparent.
        assert!(
            result.discarded.is_empty(),
            "expected no discarded vaults, got {:?}",
            result.discarded
        );
        // No repos in this merge — nothing to re-index.
        assert!(result.repos_moved.is_empty());
        assert!(!result.repos_need_reindex());

        // All 3 notes should survive under the new vault UID.
        let new_vault_uid = vault_uid("tgt", "/tmp/vault");
        let surviving = store.list_notes(Some(&new_vault_uid)).unwrap();
        assert_eq!(surviving.len(), 3, "expected 3 notes to survive merge");
    }

    #[test]
    fn merge_instance_ids_conserves_notes_across_multiple_vaults() {
        // nw-091 / Bug 4: reparent_vault is atomic per vault, so a multi-vault
        // merge (and any interruption between vaults) must never lose a note —
        // every note stays reachable under exactly one instance. Guards the
        // multi-vault loop conservation invariant.
        use nestweaver_schema::uid::vault_uid;
        use nestweaver_schema::{Note, NoteKind, Vault};
        let store = test_store();

        let mut total_notes = 0usize;
        for (v, root) in [("vlt:src:a", "/vault/a"), ("vlt:src:b", "/vault/b")] {
            store
                .insert_vault(&Vault {
                    uid: v.to_string(),
                    name: format!("vault-{v}"),
                    root_path: root.to_string(),
                    instance_id: "src".to_string(),
                })
                .unwrap();
            let notes: Vec<Note> = (0..3)
                .map(|n| Note {
                    uid: format!("note:{v}:{n}"),
                    vault_uid: v.to_string(),
                    file_path: format!("n{n}.md"),
                    title: format!("Note {n}"),
                    note_kind: NoteKind::General,
                    word_count: n,
                    content_hash: format!("h{v}{n}"),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .collect();
            total_notes += notes.len();
            store.batch_insert_notes(&notes).unwrap();
            let edges: Vec<(&str, &str)> =
                notes.iter().map(|note| (v, note.uid.as_str())).collect();
            store.batch_insert_vault_note_edges(&edges).unwrap();
        }

        let result = store.merge_instance_ids("src", "tgt").unwrap();
        assert_eq!(result.vaults, 2);
        assert!(result.discarded.is_empty(), "no vault discarded");

        // Every note is conserved under the target instance; none lost, none left
        // under the source.
        let tgt_a = store
            .list_notes(Some(&vault_uid("tgt", "/vault/a")))
            .unwrap();
        let tgt_b = store
            .list_notes(Some(&vault_uid("tgt", "/vault/b")))
            .unwrap();
        assert_eq!(
            tgt_a.len() + tgt_b.len(),
            total_notes,
            "all notes conserved across the multi-vault merge"
        );
        assert!(
            store.list_vaults(Some("src")).unwrap().is_empty(),
            "no source vault rows remain after merge"
        );
    }

    #[test]
    fn merge_instance_ids_rejects_self_merge_without_mutation() {
        use nestweaver_schema::{Note, NoteKind, Vault};
        let store = test_store();
        let vault = Vault {
            uid: "vlt:self:one".to_string(),
            name: "authored".to_string(),
            root_path: "/authored".to_string(),
            instance_id: "same".to_string(),
        };
        store.insert_vault(&vault).unwrap();
        store
            .insert_note(&Note {
                uid: "note:self:one".to_string(),
                vault_uid: vault.uid.clone(),
                file_path: "authored.md".to_string(),
                title: "Authored".to_string(),
                note_kind: NoteKind::General,
                word_count: 42,
                content_hash: "authored-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_vault_note_edge(&vault.uid, "note:self:one")
            .unwrap();

        let error = store.merge_instance_ids("same", "same").unwrap_err();
        assert!(error.to_string().contains("same"));
        assert_eq!(store.list_vaults(Some("same")).unwrap().len(), 1);
        assert_eq!(store.list_notes(Some(&vault.uid)).unwrap().len(), 1);
    }

    #[test]
    fn merge_instance_ids_collision_source_wins_preserves() {
        use nestweaver_schema::uid::vault_uid;
        use nestweaver_schema::{Note, NoteKind, Vault};
        let store = test_store();

        // Source vault with 3 notes.
        let src_vault = Vault {
            uid: "vlt:coll:src".to_string(),
            name: "shared-vault".to_string(),
            root_path: "/shared".to_string(),
            instance_id: "src".to_string(),
        };
        store.insert_vault(&src_vault).unwrap();

        let make_src_note = |uid: &str, n: u32| Note {
            uid: uid.to_string(),
            vault_uid: "vlt:coll:src".to_string(),
            file_path: format!("src{n}.md"),
            title: format!("SrcNote {n}"),
            note_kind: NoteKind::General,
            word_count: n,
            content_hash: format!("sh{n}"),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        let src_notes = vec![
            make_src_note("note:cs:1", 1),
            make_src_note("note:cs:2", 2),
            make_src_note("note:cs:3", 3),
        ];
        store.batch_insert_notes(&src_notes).unwrap();
        store
            .batch_insert_vault_note_edges(&[
                ("vlt:coll:src", "note:cs:1"),
                ("vlt:coll:src", "note:cs:2"),
                ("vlt:coll:src", "note:cs:3"),
            ])
            .unwrap();

        // Target vault with 1 note at the same root_path.
        let tgt_vault = Vault {
            uid: "vlt:coll:tgt".to_string(),
            name: "shared-vault".to_string(),
            root_path: "/shared".to_string(),
            instance_id: "tgt".to_string(),
        };
        store.insert_vault(&tgt_vault).unwrap();

        let tgt_note = Note {
            uid: "note:ct:1".to_string(),
            vault_uid: "vlt:coll:tgt".to_string(),
            file_path: "tgt1.md".to_string(),
            title: "TgtNote 1".to_string(),
            note_kind: NoteKind::General,
            word_count: 1,
            content_hash: "th1".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.insert_note(&tgt_note).unwrap();
        store
            .insert_vault_note_edge("vlt:coll:tgt", "note:ct:1")
            .unwrap();

        // Merge — source wins (3 > 1).
        let result = store.merge_instance_ids("src", "tgt").unwrap();

        // The 1 dropped target note should be reported as discarded.
        assert_eq!(result.discarded.len(), 1);
        assert_eq!(result.discarded[0].root_path, "/shared");
        assert_eq!(result.discarded[0].notes_discarded, 1);

        // All 3 source notes should survive under the new vault UID.
        let new_vault_uid = vault_uid("tgt", "/shared");
        let surviving = store.list_notes(Some(&new_vault_uid)).unwrap();
        assert_eq!(surviving.len(), 3, "expected 3 source notes to survive");

        // Only one vault should remain.
        let vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        assert_eq!(vaults[0].uid, new_vault_uid);
        assert_eq!(vaults[0].instance_id, "tgt");
    }

    /// Merging an instance removes its derived code graph rows before re-minting
    /// the Repo node under the target instance.
    #[test]
    fn merge_instance_ids_reports_repos_needing_reindex() {
        use nestweaver_schema::uid::{repo_uid, symbol_uid};
        let store = test_store();

        // A repo under instance "old" with one symbol child.
        let repo = Repo {
            uid: repo_uid("old", "https://github.com/example/svc"),
            url: "https://github.com/example/svc".to_string(),
            indexed_sha: "abc123".to_string(),
            staleness_commits_behind: 0,
            instance_id: "old".to_string(),
            name: Some("svc".to_string()),
            root_path: Some("/srv/example/svc".to_string()),
        };
        store.insert_repo(&repo).unwrap();
        // The merge plan verifier only accepts production-shaped UIDs.
        let symbol_uid = symbol_uid(&repo.uid, "src/lib.rs", "handler", 10);
        let symbol = make_symbol(&symbol_uid, "handler", &repo.uid, "src/lib.rs");
        store.insert_symbol(&symbol).unwrap();

        let report = store.merge_instance_ids("old", "new").unwrap();

        // The repo was re-minted → the caller must be told to re-index it.
        assert_eq!(report.repos, 1);
        assert_eq!(report.repos_moved.len(), 1);
        assert_eq!(report.repos_moved[0], "svc"); // display name preferred
        assert!(report.repos_need_reindex());

        assert_eq!(report.repo_uids_removed, vec![repo.uid.clone()]);
        assert!(store.lookup_symbol(&symbol_uid).is_err());
        assert!(store.lookup_repo(&repo.uid).unwrap().is_none());
        let target_uid = repo_uid("new", "https://github.com/example/svc");
        assert_eq!(
            store.lookup_repo(&target_uid).unwrap().unwrap().instance_id,
            "new"
        );
    }

    #[test]
    fn merge_instance_ids_repo_collision_preserves_target() {
        use nestweaver_schema::uid::{repo_uid, symbol_uid};
        let store = test_store();
        let url = "https://github.com/example/collision";
        let source = Repo {
            uid: repo_uid("old", url),
            url: url.to_string(),
            indexed_sha: "source-sha".to_string(),
            staleness_commits_behind: 0,
            instance_id: "old".to_string(),
            name: Some("source".to_string()),
            root_path: None,
        };
        let target = Repo {
            uid: repo_uid("new", url),
            url: url.to_string(),
            indexed_sha: "target-sha".to_string(),
            staleness_commits_behind: 0,
            instance_id: "new".to_string(),
            name: Some("target".to_string()),
            root_path: None,
        };
        store.insert_repo(&source).unwrap();
        store.insert_repo(&target).unwrap();
        // The merge plan verifier only accepts production-shaped UIDs.
        let symbol_uid = symbol_uid(&source.uid, "src/lib.rs", "handler", 10);
        store
            .insert_symbol(&make_symbol(
                &symbol_uid,
                "handler",
                &source.uid,
                "src/lib.rs",
            ))
            .unwrap();

        let report = store.merge_instance_ids("old", "new").unwrap();
        assert_eq!(report.repos, 1);
        assert_eq!(report.repo_uids_removed, vec![source.uid.clone()]);
        assert!(store.lookup_symbol(&symbol_uid).is_err());
        let surviving = store.lookup_repo(&target.uid).unwrap().unwrap();
        assert_eq!(surviving.indexed_sha, "target-sha");
        assert_eq!(store.list_repos(Some("new")).unwrap().len(), 1);
    }

    #[test]
    fn instance_uid_remap_plan_covers_code_and_project_collisions() {
        use nestweaver_schema::uid::{
            file_uid, project_uid, repo_uid, service_uid, symbol_uid, vault_uid,
        };
        use nestweaver_schema::{File, Project, Service, Vault};

        let store = test_store();
        let url = "https://github.com/example/remap";
        let source_repo_uid = repo_uid("old", url);
        let target_repo_uid = repo_uid("new", url);
        let source_repo = Repo {
            uid: source_repo_uid.clone(),
            url: url.to_string(),
            indexed_sha: "source-sha".to_string(),
            staleness_commits_behind: 0,
            instance_id: "old".to_string(),
            name: Some("source".to_string()),
            root_path: None,
        };
        let target_repo = Repo {
            uid: target_repo_uid.clone(),
            instance_id: "new".to_string(),
            indexed_sha: "target-sha".to_string(),
            name: Some("target".to_string()),
            ..source_repo.clone()
        };
        store.insert_repo(&source_repo).unwrap();
        store.insert_repo(&target_repo).unwrap();

        let source_file_uid = file_uid(&source_repo_uid, "src/lib.rs");
        store
            .insert_file(&File {
                uid: source_file_uid.clone(),
                path: "src/lib.rs".to_string(),
                repo_uid: source_repo_uid.clone(),
                content_hash: "file-hash".to_string(),
            })
            .unwrap();
        let source_symbol_uid = symbol_uid(&source_repo_uid, "src/lib.rs", "handler", 10);
        store
            .insert_symbol(&make_symbol(
                &source_symbol_uid,
                "handler",
                &source_repo_uid,
                "src/lib.rs",
            ))
            .unwrap();
        let source_service_uid = service_uid(&source_repo_uid, "api");
        store
            .insert_service(&Service {
                uid: source_service_uid.clone(),
                name: "api".to_string(),
                repo_uid: source_repo_uid.clone(),
                summary: None,
                summary_hash: None,
                embedding: None,
            })
            .unwrap();

        let vault_root = "/tmp/remap-vault";
        let source_vault_uid = vault_uid("old", vault_root);
        let target_vault_uid = vault_uid("new", vault_root);
        store
            .insert_vault(&Vault {
                uid: source_vault_uid.clone(),
                name: "Remap vault".to_string(),
                root_path: vault_root.to_string(),
                instance_id: "old".to_string(),
            })
            .unwrap();

        let source_project_uid = project_uid("old", "Roadmap");
        let target_project_uid = project_uid("new", "Roadmap");
        store
            .insert_project(&Project {
                uid: source_project_uid.clone(),
                name: "Roadmap".to_string(),
                summary: Some("source".to_string()),
                instance_id: "old".to_string(),
            })
            .unwrap();
        store
            .insert_project(&Project {
                uid: target_project_uid.clone(),
                name: "Roadmap".to_string(),
                summary: Some("target".to_string()),
                instance_id: "new".to_string(),
            })
            .unwrap();

        let plan = store.plan_instance_uid_remaps("old", "new").unwrap();
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan)
                .unwrap(),
            super::InstanceUidRemapPlanState::Prepared
        );

        assert_eq!(
            plan,
            vec![
                InstanceUidRemap {
                    source_uid: source_file_uid,
                    destination_uid: file_uid(&target_repo_uid, "src/lib.rs"),
                },
                InstanceUidRemap {
                    source_uid: source_project_uid,
                    destination_uid: target_project_uid,
                },
                InstanceUidRemap {
                    source_uid: source_repo_uid,
                    destination_uid: target_repo_uid.clone(),
                },
                InstanceUidRemap {
                    source_uid: source_service_uid,
                    destination_uid: service_uid(&target_repo_uid, "api"),
                },
                InstanceUidRemap {
                    source_uid: source_symbol_uid,
                    destination_uid: symbol_uid(&target_repo_uid, "src/lib.rs", "handler", 10,),
                },
                InstanceUidRemap {
                    source_uid: source_vault_uid,
                    destination_uid: target_vault_uid,
                },
            ]
        );
        store.merge_instance_ids("old", "new").unwrap();
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan)
                .unwrap(),
            super::InstanceUidRemapPlanState::Applied
        );

        let mut tampered = plan;
        tampered[0].destination_uid = "file:repo:new:ffffffffffff:ffffffffffff".to_string();
        assert!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &tampered)
                .is_err()
        );
    }

    #[test]
    fn instance_uid_remap_plan_recognizes_a_proven_partial_multi_repo_application() {
        use nestweaver_schema::uid::repo_uid;

        let store = test_store();
        let urls = [
            "https://github.com/example/partial-a",
            "https://github.com/example/partial-b",
        ];
        for url in urls {
            store
                .insert_repo(&Repo {
                    uid: repo_uid("old", url),
                    url: url.to_string(),
                    indexed_sha: "source".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: "old".to_string(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }
        let plan = store.plan_instance_uid_remaps("old", "new").unwrap();

        let applied_url = urls[0];
        let source_uid = repo_uid("old", applied_url);
        store
            .bulk_delete_repo_files_and_symbols(&source_uid)
            .unwrap();
        store.clear_repo_derived_nodes(&source_uid).unwrap();
        store.delete_repo_node(&source_uid).unwrap();
        store
            .insert_repo(&Repo {
                uid: repo_uid("new", applied_url),
                url: applied_url.to_string(),
                indexed_sha: "source".to_string(),
                staleness_commits_behind: 0,
                instance_id: "new".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan)
                .unwrap(),
            super::InstanceUidRemapPlanState::PartiallyApplied
        );
    }

    #[test]
    fn instance_uid_remap_plan_recovers_repo_deleted_before_destination_insert() {
        use nestweaver_schema::uid::repo_uid;

        let store = test_store();
        let url = "https://github.com/example/repo-insert-crash";
        let source_uid = repo_uid("old", url);
        let destination_uid = repo_uid("new", url);
        store
            .insert_repo(&Repo {
                uid: source_uid.clone(),
                url: url.to_string(),
                indexed_sha: "source-sha".to_string(),
                staleness_commits_behind: 2,
                instance_id: "old".to_string(),
                name: Some("recover-me".to_string()),
                root_path: Some("/tmp/recover-me".to_string()),
            })
            .unwrap();
        let plan = store.plan_instance_uid_migration("old", "new").unwrap();

        store
            .bulk_delete_repo_files_and_symbols(&source_uid)
            .unwrap();
        store.clear_repo_derived_nodes(&source_uid).unwrap();
        store.delete_repo_node(&source_uid).unwrap();

        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan.remaps)
                .unwrap(),
            super::InstanceUidRemapPlanState::PartiallyApplied
        );
        assert_eq!(
            store
                .recover_missing_instance_repos("new", &plan.repo_recoveries)
                .unwrap(),
            1
        );
        let recovered = store.lookup_repo(&destination_uid).unwrap().unwrap();
        assert_eq!(recovered.url, url);
        assert_eq!(recovered.name.as_deref(), Some("recover-me"));
        assert_eq!(recovered.root_path.as_deref(), Some("/tmp/recover-me"));
        assert_eq!(recovered.indexed_sha, "");
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan.remaps)
                .unwrap(),
            super::InstanceUidRemapPlanState::Applied
        );
    }

    #[test]
    fn instance_uid_remap_plan_recovers_vault_and_project_deleted_before_destination_insert() {
        use nestweaver_schema::uid::{project_uid, vault_uid};
        use nestweaver_schema::{Project, Vault};

        let store = test_store();
        let vault_root = "/tmp/vault-insert-crash";
        let source_vault_uid = vault_uid("old", vault_root);
        let destination_vault_uid = vault_uid("new", vault_root);
        store
            .insert_vault(&Vault {
                uid: source_vault_uid.clone(),
                name: "Recover vault".to_string(),
                root_path: vault_root.to_string(),
                instance_id: "old".to_string(),
            })
            .unwrap();

        let source_project_uid = project_uid("old", "Recover project");
        let destination_project_uid = project_uid("new", "Recover project");
        store
            .insert_project(&Project {
                uid: source_project_uid.clone(),
                name: "Recover project".to_string(),
                summary: Some("source summary".to_string()),
                instance_id: "old".to_string(),
            })
            .unwrap();

        let plan = store.plan_instance_uid_migration("old", "new").unwrap();
        store.delete_vault_cascade(&source_vault_uid).unwrap();
        store.delete_project_node(&source_project_uid).unwrap();

        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan.remaps)
                .unwrap(),
            super::InstanceUidRemapPlanState::PartiallyApplied
        );
        assert_eq!(
            store
                .recover_missing_instance_roots(
                    "new",
                    &plan.repo_recoveries,
                    &plan.vault_recoveries,
                    &plan.project_recoveries,
                )
                .unwrap(),
            2
        );

        let recovered_vault = store
            .list_vaults(None)
            .unwrap()
            .into_iter()
            .find(|vault| vault.uid == destination_vault_uid)
            .expect("destination vault should be restored");
        assert_eq!(recovered_vault.name, "Recover vault");
        assert_eq!(recovered_vault.root_path, vault_root);
        assert_eq!(recovered_vault.instance_id, "new");
        let recovered_project = store
            .list_projects()
            .unwrap()
            .into_iter()
            .find(|project| project.uid == destination_project_uid)
            .expect("destination project should be restored");
        assert_eq!(recovered_project.summary.as_deref(), Some("source summary"));
        assert_eq!(recovered_project.instance_id, "new");
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("old", "new", &plan.remaps)
                .unwrap(),
            super::InstanceUidRemapPlanState::Applied
        );
    }

    #[test]
    fn project_collision_plan_and_merge_are_order_independent() {
        use nestweaver_schema::Project;

        type ProjectSnapshot = (String, String, Option<String>, String);
        type RunResult = (Vec<InstanceUidRemap>, Vec<ProjectSnapshot>);

        fn run(reverse: bool) -> RunResult {
            let store = test_store();
            let target_winner_uid = "proj:new:000000000001";
            let target_loser_uid = "proj:new:ffffffffffff";
            let mut projects = vec![
                Project {
                    uid: target_winner_uid.to_string(),
                    name: "Roadmap".to_string(),
                    summary: Some("stable target winner".to_string()),
                    instance_id: "new".to_string(),
                },
                Project {
                    uid: target_loser_uid.to_string(),
                    name: "ROADMAP".to_string(),
                    summary: Some("legacy target loser".to_string()),
                    instance_id: "new".to_string(),
                },
                Project {
                    uid: "proj:old:111111111111".to_string(),
                    name: "roadmap".to_string(),
                    summary: Some("source lower".to_string()),
                    instance_id: "old".to_string(),
                },
                Project {
                    uid: "proj:old:222222222222".to_string(),
                    name: "RoadMap".to_string(),
                    summary: Some("source mixed".to_string()),
                    instance_id: "old".to_string(),
                },
            ];
            if reverse {
                projects.reverse();
            }
            for project in projects {
                store.insert_project(&project).unwrap();
            }

            let plan = store.plan_instance_uid_remaps("old", "new").unwrap();
            let result = store.merge_instance_ids("old", "new").unwrap();
            assert_eq!(result.projects, 2);
            let mut surviving: Vec<_> = store
                .list_projects()
                .unwrap()
                .into_iter()
                .map(|project| {
                    (
                        project.uid,
                        project.name,
                        project.summary,
                        project.instance_id,
                    )
                })
                .collect();
            surviving.sort();
            (plan, surviving)
        }

        let (forward_plan, forward_projects) = run(false);
        let (reverse_plan, reverse_projects) = run(true);
        let winner = "proj:new:000000000001";
        let expected_plan = vec![
            InstanceUidRemap {
                source_uid: "proj:new:ffffffffffff".to_string(),
                destination_uid: winner.to_string(),
            },
            InstanceUidRemap {
                source_uid: "proj:old:111111111111".to_string(),
                destination_uid: winner.to_string(),
            },
            InstanceUidRemap {
                source_uid: "proj:old:222222222222".to_string(),
                destination_uid: winner.to_string(),
            },
        ];
        assert_eq!(forward_plan, expected_plan);
        assert_eq!(reverse_plan, expected_plan);
        assert_eq!(forward_projects, reverse_projects);
        assert_eq!(forward_projects.len(), 1);
        assert_eq!(forward_projects[0].0, winner);
        assert_eq!(forward_projects[0].1, "Roadmap");
        assert_eq!(
            forward_projects[0].2.as_deref(),
            Some("stable target winner")
        );
    }

    #[test]
    fn project_source_only_case_variants_choose_lexical_reminted_uid() {
        use nestweaver_schema::Project;
        use nestweaver_schema::uid::project_uid;

        type ProjectSnapshot = (String, String, Option<String>);
        type RunResult = (Vec<InstanceUidRemap>, Vec<ProjectSnapshot>);

        fn run(reverse: bool) -> RunResult {
            let store = test_store();
            let mut projects = vec![
                Project {
                    uid: "proj:old:aaaaaaaaaaaa".to_string(),
                    name: "Alpha".to_string(),
                    summary: Some("upper".to_string()),
                    instance_id: "old".to_string(),
                },
                Project {
                    uid: "proj:old:bbbbbbbbbbbb".to_string(),
                    name: "alpha".to_string(),
                    summary: Some("lower".to_string()),
                    instance_id: "old".to_string(),
                },
            ];
            if reverse {
                projects.reverse();
            }
            for project in projects {
                store.insert_project(&project).unwrap();
            }
            let plan = store.plan_instance_uid_remaps("old", "new").unwrap();
            store.merge_instance_ids("old", "new").unwrap();
            let mut projects: Vec<_> = store
                .list_projects()
                .unwrap()
                .into_iter()
                .map(|project| (project.uid, project.name, project.summary))
                .collect();
            projects.sort();
            (plan, projects)
        }

        let upper_uid = project_uid("new", "Alpha");
        let lower_uid = project_uid("new", "alpha");
        let expected_winner = upper_uid.min(lower_uid);
        let (forward_plan, forward_projects) = run(false);
        let (reverse_plan, reverse_projects) = run(true);

        assert_eq!(forward_plan, reverse_plan);
        assert_eq!(forward_projects, reverse_projects);
        assert_eq!(forward_projects.len(), 1);
        assert_eq!(forward_projects[0].0, expected_winner);
        assert!(
            forward_plan
                .iter()
                .all(|mapping| mapping.destination_uid == expected_winner)
        );
        assert_eq!(forward_plan.len(), 2);
    }
}

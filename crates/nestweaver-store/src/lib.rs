pub mod cache;
pub mod db;
pub mod error;
pub mod generation;
pub mod ranking;
pub mod read;
pub mod regex;
pub mod search;
pub mod tantivy_index;
pub mod traverse;
pub mod write;

pub use db::GraphStore;
pub use error::StoreError;
pub use ranking::{
    DEFAULT_GIT_ACTIVITY_WEIGHT, GIT_ACTIVITY_MULT_MAX, GIT_ACTIVITY_MULT_MIN, GraphScope,
    QueryIntent, ScopedEdgeQuery, detect_intent, git_activity_multiplier,
};
pub use read::{
    BacklinkRow, BrokenWikilinkRow, CodeEdge, CodeGraph, CrossRepoRef, NoteLite, SymbolBasic,
};
pub use regex::{
    CANDIDATE_CAP, DEFAULT_MAX_MILLIS, FileCount, PatternCount, RegexMatch, RegexSearchResult,
};
pub use search::{EmbeddingIndex, SearchResult};
pub use tantivy_index::{
    PRF_EXPANSION_TERMS, PRF_EXPANSION_WEIGHT, PRF_MAX_QUERY_TERMS, PRF_TOP_K, SearchHit,
    TantivyError, TantivyIndex,
};
pub use traverse::ImpactNode;

#[cfg(test)]
mod tests {
    use nestweaver_schema::{
        EdgeType, File, Repo, ResolvedEdge, Service, Symbol, SymbolKind, Visibility,
    };

    use super::GraphStore;

    fn make_repo(uid: &str) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: format!("https://github.com/example/{uid}"),
            indexed_sha: "abc123".to_string(),
            staleness_commits_behind: 0,
            instance_id: "inst-1".to_string(),
            name: None,
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
        }
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
            })
            .unwrap();

        // Should succeed — both endpoints exist.
        store.insert_vault_note_edge("vlt:x", "note:x:1").unwrap();
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
        assert_eq!(deleted, 1);

        // sym-a should be gone; sym-b should remain.
        let err = store.lookup_symbol("sym-del-a").unwrap_err();
        assert!(matches!(err, crate::StoreError::NotFound));
        let still_there = store.lookup_symbol("sym-del-b").unwrap();
        assert_eq!(still_there.uid, "sym-del-b");
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
        let wikilink_to_note_edges: Vec<(&str, &str, f32, &str)> =
            vec![("sec:bvw:s1", "note:bvw:n2", 1.0, "Note Two")];

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
}

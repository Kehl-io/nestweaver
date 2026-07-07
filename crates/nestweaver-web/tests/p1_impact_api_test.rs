use axum::body::Body;
use axum::http::{Request, StatusCode};
use nestweaver_schema::{
    EdgeType, Project, Repo, ResolvedEdge, Symbol, SymbolKind, Vault, Visibility,
};
use nestweaver_store::{GraphScope, GraphStore};
use nestweaver_web::create_router;
use nestweaver_web::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let app = app.clone();
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn repo(uid: &str, name: &str, behind: u32) -> Repo {
    Repo {
        uid: uid.to_string(),
        url: format!("https://example.com/{name}.git"),
        indexed_sha: format!("{name}-sha"),
        staleness_commits_behind: behind,
        instance_id: "local".to_string(),
        name: Some(name.to_string()),
        root_path: Some(format!("/tmp/{name}")),
    }
}

fn project(uid: &str, name: &str) -> Project {
    Project {
        uid: uid.to_string(),
        name: name.to_string(),
        summary: Some(format!("{name} workspace")),
        instance_id: "local".to_string(),
    }
}

fn symbol(uid: &str, repo_uid: &str, name: &str, file_path: &str, line: u32) -> Symbol {
    Symbol {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: SymbolKind::Function,
        repo_uid: repo_uid.to_string(),
        file_path: file_path.to_string(),
        start_line: line,
        end_line: line + 2,
        signature: format!("fn {name}()"),
        summary: None,
        content_hash: format!("{uid}-hash"),
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

fn calls_edge(source_uid: &str, target_uid: &str, confidence: f32) -> ResolvedEdge {
    ResolvedEdge {
        source_uid: source_uid.to_string(),
        target_uid: target_uid.to_string(),
        edge_type: EdgeType::Calls,
        confidence,
        link_type: None,
        evidence: Vec::new(),
    }
}

fn make_app() -> axum::Router {
    let store = GraphStore::in_memory().unwrap();
    let impact_repo = repo("repo:impact", "impact", 2);
    let other_repo = repo("repo:other", "other", 0);
    store.insert_repo(&impact_repo).unwrap();
    store.insert_repo(&other_repo).unwrap();

    store
        .insert_symbol(&symbol(
            "sym:impact:target",
            &impact_repo.uid,
            "target_logic",
            "src/target.rs",
            10,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:impact:caller",
            &impact_repo.uid,
            "caller_logic",
            "src/caller.rs",
            20,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:impact:test",
            &impact_repo.uid,
            "target_logic_test",
            "tests/target_logic_test.rs",
            30,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:other:caller",
            &other_repo.uid,
            "other_caller",
            "src/other_caller.rs",
            40,
        ))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:impact:caller", "sym:impact:target", 0.9))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:impact:test", "sym:impact:caller", 0.8))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:other:caller", "sym:impact:target", 0.95))
        .unwrap();

    let project = project("proj:impact-core", "Impact Core");
    store.insert_project(&project).unwrap();
    store
        .batch_insert_project_symbol_edges(
            &project.uid,
            &[
                "sym:impact:target".to_string(),
                "sym:impact:caller".to_string(),
            ],
            1.0,
        )
        .unwrap();
    store
        .insert_vault(&Vault {
            uid: "vlt:impact-notes".to_string(),
            name: "Impact Notes".to_string(),
            root_path: "/tmp/impact-notes".to_string(),
            instance_id: "local".to_string(),
        })
        .unwrap();
    store
        .compute_pagerank(0.85, 20, &GraphScope::code_only())
        .unwrap();

    let state = AppState::new(
        store,
        None,
        std::path::PathBuf::from("/tmp/p1-impact-test.lbug"),
    );
    create_router(state)
}

#[tokio::test]
async fn p1_impact_returns_layered_envelope_with_tests_evidence_and_meta() {
    let app = make_app();
    let (status, json) = get_json(
        &app,
        "/api/v1/impact/sym:impact:target?depth=3&confidence=0.3&workspace=repo:repo:impact",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(json["target"]["uid"], "sym:impact:target");
    assert_eq!(json["target"]["layer"], 0);
    assert_eq!(json["target"]["source"]["file_path"], "src/target.rs");
    assert!(
        json["target"]["source"]["url"]
            .as_str()
            .expect("target source url should be present")
            .contains("/api/v1/source?file=src%2Ftarget.rs&line=10"),
        "target should include a source evidence link"
    );

    let nodes = json["nodes"].as_array().expect("nodes should be an array");
    assert!(
        nodes
            .iter()
            .any(|node| node["uid"] == "sym:impact:caller" && node["layer"] == 1),
        "direct caller should be present in layer 1"
    );
    assert!(
        nodes
            .iter()
            .any(|node| node["uid"] == "sym:impact:test" && node["layer"] == 2),
        "transitive affected test should be present in layer 2"
    );

    let edges = json["edges"].as_array().expect("edges should be an array");
    assert!(
        edges
            .iter()
            .any(|edge| edge["source"] == "sym:impact:caller"
                && edge["target"] == "sym:impact:target"
                && edge["edge_type"] == "CALLS"),
        "DAG edges should include the direct caller relationship"
    );
    assert!(
        edges.iter().any(
            |edge| edge["source"] == "sym:impact:test" && edge["target"] == "sym:impact:caller"
        ),
        "DAG edges should preserve layered transitive relationships"
    );

    assert_eq!(
        json["affected_tests"]["changed_files"][0], "src/target.rs",
        "affected tests should be derived from the target source file"
    );
    assert!(
        json["affected_tests"]["tier_2"]
            .as_array()
            .expect("tier_2 should be an array")
            .iter()
            .any(|test| test["test_file"] == "tests/target_logic_test.rs"),
        "affected tests should surface static RTS hints"
    );
    assert!(
        json["affected_tests"]["disclaimer"]
            .as_str()
            .expect("disclaimer should be present")
            .contains("NOT a provably-safe subset")
    );

    assert_eq!(json["states"]["tier"], "local-only");
    assert_eq!(json["states"]["org"], "org-unavailable");
    assert_eq!(json["states"]["freshness"], "stale");
    assert_eq!(json["states"]["timeout"], "unknown");
    assert_eq!(json["states"]["permission"], "unknown");
    assert_eq!(json["states"]["read_only"], "unknown");
    assert_eq!(json["states"]["result"], "partial");

    assert_eq!(json["_meta"]["workspace_type"], "repo");
    assert_eq!(json["_meta"]["trust"]["federation"], "local-only");
    assert_eq!(json["_meta"]["trust"]["freshness"], "stale");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .expect("unsupported states should be an array")
            .iter()
            .any(|item| item == "org-wide-impact"),
        "meta should disclose unavailable org-wide/two-tier impact"
    );
    assert!(
        !nodes.iter().any(|node| node["uid"] == "sym:other:caller"),
        "repo-scoped impact should not include callers from another repo"
    );
}

#[tokio::test]
async fn p1_impact_project_scope_filters_symbols_and_affected_tests() {
    let app = make_app();
    let (status, json) = get_json(
        &app,
        "/api/v1/impact/sym:impact:target?depth=3&confidence=0.3&workspace=project:proj:impact-core",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["workspace_type"], "project");
    assert_eq!(json["_meta"]["trust"]["data_scope"], "project-scoped");
    assert_eq!(json["states"]["result"], "partial");

    let nodes = json["nodes"].as_array().expect("nodes should be an array");
    assert!(
        nodes.iter().any(|node| node["uid"] == "sym:impact:caller"),
        "project-scoped impact should include project member callers"
    );
    assert!(
        !nodes
            .iter()
            .any(|node| { node["uid"] == "sym:impact:test" || node["uid"] == "sym:other:caller" }),
        "project-scoped impact should exclude symbols outside the project membership"
    );
    assert!(
        json["affected_tests"]["tier_2"]
            .as_array()
            .expect("tier_2 should be an array")
            .is_empty(),
        "project-scoped affected tests should not leak non-project test symbols"
    );
}

#[tokio::test]
async fn p1_impact_repo_scope_returns_no_match_for_out_of_scope_target() {
    let app = make_app();
    let (status, json) = get_json(
        &app,
        "/api/v1/impact/sym:other:caller?depth=3&confidence=0.3&workspace=repo:repo:impact",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["target"].is_null());
    assert!(json["nodes"].as_array().unwrap().is_empty());
    assert!(json["edges"].as_array().unwrap().is_empty());
    assert_eq!(json["_meta"]["workspace_type"], "repo");
    assert_eq!(json["_meta"]["trust"]["result"], "no-match");
    assert_eq!(json["states"]["result"], "no-match");
    assert!(
        json["_meta"]["trust"]["message"]
            .as_str()
            .unwrap()
            .contains("no out-of-scope impact data was returned"),
        "out-of-scope target should be explicit"
    );
}

#[tokio::test]
async fn p1_impact_vault_scope_is_explicitly_unsupported() {
    let app = make_app();
    let (status, json) = get_json(
        &app,
        "/api/v1/impact/sym:impact:target?depth=3&confidence=0.3&workspace=vault:vlt:impact-notes",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["target"].is_null());
    assert!(json["nodes"].as_array().unwrap().is_empty());
    assert_eq!(json["_meta"]["workspace_type"], "vault");
    assert_eq!(json["_meta"]["trust"]["result"], "unsupported");
    assert_eq!(json["_meta"]["trust"]["freshness"], "unknown");
    assert_eq!(json["states"]["tier"], "unsupported");
    assert_eq!(json["states"]["timeout"], "not-applicable");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "two-tier-impact"),
        "vault-scoped unsupported impact should still disclose unavailable two-tier state"
    );
}

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use nestweaver_schema::{
    EdgeType, Note, NoteKind, Project, Repo, ResolvedEdge, Service, Symbol, SymbolKind, Vault,
    Visibility,
};
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};
use nestweaver_web::create_router;
use nestweaver_web::state::AppState;
use serde_json::{Value, json};
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

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn repo(uid: &str, name: &str) -> Repo {
    Repo {
        uid: uid.to_string(),
        url: format!("https://example.com/{name}.git"),
        indexed_sha: format!("{name}-sha"),
        staleness_commits_behind: 0,
        instance_id: "local".to_string(),
        name: Some(name.to_string()),
        root_path: Some(format!("/tmp/{name}")),
    }
}

fn symbol(uid: &str, repo_uid: &str, name: &str, score: f64) -> Symbol {
    Symbol {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: SymbolKind::Function,
        repo_uid: repo_uid.to_string(),
        file_path: format!("src/{name}.rs"),
        start_line: 1,
        end_line: 3,
        signature: format!("fn {name}()"),
        summary: None,
        content_hash: format!("{name}-hash"),
        embedding: None,
        pagerank_score: Some(score),
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    }
}

fn note(uid: &str, vault_uid: &str, title: &str, score: f64) -> Note {
    Note {
        uid: uid.to_string(),
        vault_uid: vault_uid.to_string(),
        file_path: format!("{title}.md"),
        title: title.to_string(),
        note_kind: NoteKind::General,
        word_count: 25,
        content_hash: format!("{title}-hash"),
        frontmatter: None,
        frontmatter_raw: None,
        created_at: None,
        modified_at: None,
        pagerank_score: Some(score),
        embedding: None,
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

fn calls_edge(source_uid: &str, target_uid: &str) -> ResolvedEdge {
    ResolvedEdge {
        source_uid: source_uid.to_string(),
        target_uid: target_uid.to_string(),
        edge_type: EdgeType::Calls,
        confidence: 1.0,
        link_type: None,
        evidence: Vec::new(),
    }
}

fn setup_p1_store() -> GraphStore {
    let store = GraphStore::in_memory().unwrap();

    let repo_alpha = repo("repo:alpha", "alpha");
    let repo_beta = repo("repo:beta", "beta");
    store.insert_repo(&repo_alpha).unwrap();
    store.insert_repo(&repo_beta).unwrap();

    store
        .insert_service(&Service {
            uid: "svc:alpha:web".to_string(),
            name: "alpha-web".to_string(),
            repo_uid: repo_alpha.uid.clone(),
            summary: None,
            summary_hash: None,
            embedding: None,
        })
        .unwrap();
    store
        .insert_service(&Service {
            uid: "svc:beta:worker".to_string(),
            name: "beta-worker".to_string(),
            repo_uid: repo_beta.uid.clone(),
            summary: None,
            summary_hash: None,
            embedding: None,
        })
        .unwrap();

    store
        .insert_symbol(&symbol(
            "sym:alpha:parse",
            &repo_alpha.uid,
            "parse_alpha",
            0.95,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:alpha:format",
            &repo_alpha.uid,
            "format_alpha",
            0.9,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:beta:parse",
            &repo_beta.uid,
            "parse_beta",
            0.85,
        ))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:alpha:parse", "sym:alpha:format"))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:alpha:parse", "sym:beta:parse"))
        .unwrap();

    let vault = Vault {
        uid: "vlt:brain".to_string(),
        name: "Brain".to_string(),
        root_path: "/tmp/brain".to_string(),
        instance_id: "local".to_string(),
    };
    store.insert_vault(&vault).unwrap();
    store
        .insert_note(&note("note:brain:alpha", &vault.uid, "Alpha Note", 0.92))
        .unwrap();
    store
        .insert_note(&note("note:brain:beta", &vault.uid, "Beta Note", 0.82))
        .unwrap();

    let alpha_project = project("proj:local:alpha", "Alpha Project");
    store.insert_project(&alpha_project).unwrap();
    store
        .batch_insert_project_symbol_edges(
            &alpha_project.uid,
            &[
                "sym:alpha:parse".to_string(),
                "sym:alpha:format".to_string(),
            ],
            1.0,
        )
        .unwrap();
    store
        .batch_insert_project_note_edges(&[(&alpha_project.uid, "note:brain:alpha")])
        .unwrap();

    store
        .compute_pagerank(0.85, 20, &GraphScope::unified())
        .unwrap();

    store
}

fn make_app() -> axum::Router {
    let store = setup_p1_store();
    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/p1-test.lbug"));
    create_router(state)
}

fn make_app_with_tantivy_vault_scope_fixture() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let alpha_root = dir.path().join("alpha-vault");
    let beta_root = dir.path().join("beta-vault");
    std::fs::create_dir_all(&alpha_root).unwrap();
    std::fs::create_dir_all(&beta_root).unwrap();
    std::fs::write(alpha_root.join("Needle Alpha.md"), "needle").unwrap();
    std::fs::write(beta_root.join("Needle Beta.md"), "needle ".repeat(200)).unwrap();
    // Notes whose *bodies* (not titles or paths) match a query, to prove
    // scoped search covers full text when Tantivy is present.
    std::fs::write(
        alpha_root.join("Alpha Recipe.md"),
        "zucchini bread instructions",
    )
    .unwrap();
    std::fs::write(
        beta_root.join("Beta Recipe.md"),
        "zucchini muffin instructions",
    )
    .unwrap();

    let store = GraphStore::in_memory().unwrap();
    let alpha_vault = Vault {
        uid: "vlt:alpha".to_string(),
        name: "Alpha Vault".to_string(),
        root_path: alpha_root.to_string_lossy().to_string(),
        instance_id: "local".to_string(),
    };
    let beta_vault = Vault {
        uid: "vlt:beta".to_string(),
        name: "Beta Vault".to_string(),
        root_path: beta_root.to_string_lossy().to_string(),
        instance_id: "local".to_string(),
    };
    store.insert_vault(&alpha_vault).unwrap();
    store.insert_vault(&beta_vault).unwrap();
    store
        .insert_note(&note(
            "note:alpha:needle",
            &alpha_vault.uid,
            "Needle Alpha",
            0.4,
        ))
        .unwrap();
    store
        .insert_note(&note(
            "note:beta:needle",
            &beta_vault.uid,
            "Needle Beta",
            0.9,
        ))
        .unwrap();
    store
        .insert_note(&note(
            "note:alpha:recipe",
            &alpha_vault.uid,
            "Alpha Recipe",
            0.5,
        ))
        .unwrap();
    store
        .insert_note(&note(
            "note:beta:recipe",
            &beta_vault.uid,
            "Beta Recipe",
            0.6,
        ))
        .unwrap();

    let tantivy = TantivyIndex::open_or_create(&dir.path().join("tantivy")).unwrap();
    tantivy.reindex_from_store(&store).unwrap();
    let state = AppState::new(
        store,
        Some(tantivy),
        dir.path().join("p1-tantivy-test.lbug"),
    );
    (create_router(state), dir)
}

#[tokio::test]
async fn p1_workspace_catalog_shape_includes_all_repo_and_vault_entries() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(status, StatusCode::OK);

    let workspaces = json["workspaces"]
        .as_array()
        .expect("workspaces should be an array");
    assert!(
        workspaces.iter().any(|item| item["id"] == "all"
            && item["type"] == "all"
            && item["_meta"]["trust"]["data_scope"] == "all"
            && item["_meta"]["trust"]["federation"] == "local-only"),
        "catalog should include the all-content workspace with local-only metadata"
    );

    let repo_entry = workspaces
        .iter()
        .find(|item| item["uid"] == "repo:alpha")
        .expect("repo workspace should be present");
    assert_eq!(repo_entry["type"], "repo");
    assert_eq!(repo_entry["counts"]["repo_count"], 1);
    assert_eq!(repo_entry["counts"]["symbol_count"], 2);
    assert_eq!(repo_entry["_meta"]["workspace_id"], repo_entry["id"]);
    assert_eq!(repo_entry["_meta"]["trust"]["data_scope"], "repo-scoped");

    let vault_entry = workspaces
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .expect("vault workspace should be present");
    assert_eq!(vault_entry["type"], "vault");
    assert_eq!(vault_entry["counts"]["vault_count"], 1);
    assert_eq!(vault_entry["counts"]["note_count"], 2);
    assert_eq!(vault_entry["_meta"]["trust"]["data_scope"], "vault-scoped");

    let project_entry = workspaces
        .iter()
        .find(|item| item["uid"] == "proj:local:alpha")
        .expect("project workspace should be present");
    assert_eq!(project_entry["type"], "project");
    assert_eq!(project_entry["counts"]["project_count"], 1);
    assert_eq!(project_entry["counts"]["symbol_count"], 2);
    assert_eq!(project_entry["counts"]["note_count"], 1);
    assert_eq!(
        project_entry["_meta"]["trust"]["data_scope"],
        "project-scoped"
    );
}

#[tokio::test]
async fn p1_workspace_project_scoped_overview_uses_project_membership() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let project_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "proj:local:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/overview?limit=20&workspace={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["counts"]["project_count"], 1);
    assert_eq!(json["counts"]["symbol_count"], 2);
    assert_eq!(json["counts"]["note_count"], 1);
    assert_eq!(json["_meta"]["workspace_type"], "project");
    assert_eq!(json["_meta"]["trust"]["data_scope"], "project-scoped");

    let landmarks = json["landmarks"].as_array().unwrap();
    assert!(
        landmarks
            .iter()
            .any(|item| item["uid"] == "sym:alpha:parse"),
        "project-scoped overview should include member symbols"
    );
    assert!(
        landmarks
            .iter()
            .any(|item| item["uid"] == "note:brain:alpha"),
        "project-scoped overview should include member notes"
    );
    assert!(
        !landmarks
            .iter()
            .any(|item| item["uid"] == "sym:beta:parse" || item["uid"] == "note:brain:beta"),
        "project-scoped overview should not leak non-member content"
    );
}

#[tokio::test]
async fn p1_workspace_repo_scoped_overview_does_not_silently_ignore_scope() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let repo_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "repo:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/overview?limit=20&workspace={repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["counts"]["repo_count"], 1);
    assert_eq!(json["counts"]["service_count"], 1);
    assert_eq!(json["counts"]["symbol_count"], 2);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "repo-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "note-landmarks"),
        "repo overview should disclose that note landmarks are not repo-scoped"
    );

    let landmarks = json["landmarks"].as_array().unwrap();
    assert!(
        landmarks
            .iter()
            .any(|item| item["uid"] == "sym:alpha:parse"),
        "repo-scoped overview should include symbols from the requested repo"
    );
    assert!(
        !landmarks
            .iter()
            .any(|item| item["uid"] == "sym:beta:parse" || item["uid"] == "repo:beta"),
        "repo-scoped overview should not leak other repos"
    );
}

#[tokio::test]
async fn p1_workspace_vault_scoped_overview_marks_code_portions_unsupported() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/overview?limit=20&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["counts"]["vault_count"], 1);
    assert_eq!(json["counts"]["note_count"], 2);
    assert_eq!(json["counts"]["symbol_count"], 0);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "code-landmarks"),
        "vault overview should explicitly mark unscoped code landmarks unsupported"
    );

    let landmarks = json["landmarks"].as_array().unwrap();
    assert!(
        landmarks.iter().any(|item| item["kind"] == "note"),
        "vault-scoped overview should include note landmarks"
    );
    assert!(
        !landmarks
            .iter()
            .any(|item| item["kind"] == "repo" || item["kind"] == "symbol"),
        "vault-scoped overview should not include code landmarks"
    );
}

#[tokio::test]
async fn p1_workspace_overview_limit_reports_truncation_metadata() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/overview?limit=1").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(json["landmarks"].as_array().unwrap().len(), 6);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "all");
    assert_eq!(json["_meta"]["trust"]["result"], "truncated");
    assert_eq!(json["_meta"]["truncation"]["truncated"], true);
    assert_eq!(json["_meta"]["truncation"]["limit"], 6);
    assert_eq!(json["_meta"]["truncation"]["omitted_count"], 3);
    assert_eq!(json["_meta"]["continuation"]["has_more"], true);
    assert_eq!(json["_meta"]["continuation"]["reason"], "result-limit");
}

#[tokio::test]
async fn p1_workspace_brain_context_repo_scope_filters_seeds_and_connected_results() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let repo_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "repo:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = post_json(
        &app,
        "/api/v1/brain/context",
        json!({ "seeds": ["sym:alpha:parse"], "workspace": repo_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "repo-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "note-results"),
        "repo-scoped brain context should disclose that note results are not repo-scoped"
    );

    let seeds = json["seeds"].as_array().unwrap();
    assert!(
        seeds.iter().any(|item| item["uid"] == "sym:alpha:parse"),
        "repo-scoped brain context should keep resolved seed symbols from the requested repo"
    );
    assert!(
        seeds.iter().all(|item| item["uid"]
            .as_str()
            .is_some_and(|uid| uid.starts_with("sym:alpha:"))),
        "repo-scoped brain context seeds should not leak outside the requested repo: {seeds:?}"
    );

    let connected = json["connected"].as_array().unwrap();
    assert!(
        connected
            .iter()
            .any(|item| item["uid"] == "sym:alpha:format"),
        "repo-scoped brain context should keep connected symbols from the requested repo"
    );
    assert!(
        !connected.iter().any(|item| item["uid"] == "sym:beta:parse"),
        "repo-scoped brain context should remove connected symbols from other repos"
    );
}

#[tokio::test]
async fn p1_workspace_brain_context_empty_after_scope_filter_uses_no_match_metadata() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = post_json(
        &app,
        "/api/v1/brain/context",
        json!({ "seeds": ["sym:missing:parse"], "workspace": vault_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["seeds"].as_array().unwrap().is_empty());
    assert!(json["connected"].as_array().unwrap().is_empty());
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "no-match");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "code-results"),
        "vault-scoped empty context should still disclose unsupported code results"
    );
}

#[tokio::test]
async fn p1_workspace_brain_context_project_scope_filters_member_results() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let project_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "proj:local:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = post_json(
        &app,
        "/api/v1/brain/context",
        json!({ "seeds": ["sym:alpha:parse"], "workspace": project_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "project-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");

    let seeds = json["seeds"].as_array().unwrap();
    assert!(
        seeds.iter().any(|item| item["uid"] == "sym:alpha:parse"),
        "project-scoped context should keep member seed symbols"
    );

    let connected = json["connected"].as_array().unwrap();
    assert!(
        connected
            .iter()
            .any(|item| item["uid"] == "sym:alpha:format"),
        "project-scoped context should include member connected symbols"
    );
    assert!(
        !connected.iter().any(|item| item["uid"] == "sym:beta:parse"),
        "project-scoped context should remove non-member connected symbols"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_vault_scope_filters_results_with_metadata() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Alpha%20Note&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    // Without Tantivy, the fallback matches note titles/paths only, so
    // coverage must never be reported as complete.
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "note-body-search"),
        "substring fallback should disclose that note bodies are not searched"
    );
    let results = json["results"].as_array().unwrap();
    assert!(
        results.iter().all(|item| item["vault_uid"] == "vlt:brain"),
        "vault-scoped brain search should return only notes from the selected vault"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_vault_scope_limits_after_scope_with_tantivy_present() {
    let (app, _dir) = make_app_with_tantivy_vault_scope_fixture();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Needle&limit=1&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["provenance"][0]["source"], "local_tantivy");

    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["uid"], "note:alpha:needle");
    assert!(
        results.iter().all(|item| item["vault_uid"] == "vlt:alpha"),
        "vault-scoped search should apply scope before limit even when Tantivy is present: {results:?}"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_repo_scope_returns_scoped_symbols_with_metadata() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let repo_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "repo:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=parse&workspace={repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "repo-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "partial");
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "note-search"),
        "repo-scoped brain search should disclose that note search is unsupported"
    );

    let results = json["results"].as_array().unwrap();
    assert!(
        results.iter().any(|item| item["uid"] == "sym:alpha:parse"),
        "repo-scoped brain search should return matching symbols from the selected repo"
    );
    assert!(
        results.iter().all(|item| item["repo_uid"] == "repo:alpha"),
        "repo-scoped brain search should not ignore workspace scope: {results:?}"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_project_scope_returns_member_symbols_and_notes() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let project_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "proj:local:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, symbol_json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=parse&workspace={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        symbol_json["_meta"]["trust"]["data_scope"],
        "project-scoped"
    );
    assert_eq!(symbol_json["_meta"]["trust"]["result"], "partial");
    let symbol_results = symbol_json["results"].as_array().unwrap();
    assert!(
        symbol_results
            .iter()
            .any(|item| item["uid"] == "sym:alpha:parse"),
        "project-scoped search should include matching member symbols"
    );
    assert!(
        !symbol_results
            .iter()
            .any(|item| item["uid"] == "sym:beta:parse"),
        "project-scoped search should not leak non-member symbols"
    );

    let (status, note_json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Alpha%20Note&workspace={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(note_json["_meta"]["trust"]["data_scope"], "project-scoped");
    let note_results = note_json["results"].as_array().unwrap();
    assert!(
        note_results
            .iter()
            .any(|item| item["uid"] == "note:brain:alpha"),
        "project-scoped search should include matching member notes"
    );
    assert!(
        !note_results
            .iter()
            .any(|item| item["uid"] == "note:brain:beta"),
        "project-scoped search should not leak non-member notes"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_vault_scope_no_match_uses_no_match_metadata() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Missing&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "no-match");
    assert_eq!(json["_meta"]["truncation"]["truncated"], false);
    assert!(json["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn p1_workspace_brain_search_vault_scope_limit_reports_truncation() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Note&limit=1&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["trust"]["result"], "truncated");
    assert_eq!(json["_meta"]["truncation"]["truncated"], true);
    assert_eq!(json["_meta"]["truncation"]["limit"], 1);
    assert_eq!(json["_meta"]["truncation"]["omitted_count"], 1);
    assert_eq!(json["results"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn p1_workspace_brain_search_vault_scope_finds_note_body_matches_with_tantivy() {
    let (app, _dir) = make_app_with_tantivy_vault_scope_fixture();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:alpha")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // "zucchini" appears only in note bodies, never in titles or paths.
    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=zucchini&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["_meta"]["trust"]["data_scope"], "vault-scoped");
    assert_eq!(json["_meta"]["provenance"][0]["source"], "local_tantivy");
    assert_eq!(json["_meta"]["trust"]["result"], "complete");

    let results = json["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|item| item["uid"] == "note:alpha:recipe"),
        "vault-scoped search should match note bodies via Tantivy: {results:?}"
    );
    assert!(
        !results.iter().any(|item| item["uid"] == "note:beta:recipe"),
        "vault-scoped body search should not leak notes from other vaults: {results:?}"
    );
}

#[tokio::test]
async fn p1_workspace_brain_search_limit_is_clamped() {
    let app = make_app();
    let (catalog_status, catalog) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(catalog_status, StatusCode::OK);
    let vault_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "vlt:brain")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = get_json(
        &app,
        &format!("/api/v1/brain/search?q=Note&limit=0&workspace={vault_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["results"].as_array().unwrap().len(),
        1,
        "limit=0 should be clamped to 1, not return an empty page"
    );
    assert_eq!(json["_meta"]["truncation"]["limit"], 1);
}

#[tokio::test]
async fn p1_workspace_brain_context_token_budget_is_disclosed_not_echoed() {
    let app = make_app();
    let (status, json) = post_json(
        &app,
        "/api/v1/brain/context",
        json!({ "seeds": ["sym:alpha:parse"], "token_budget": 4000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        json["_meta"]["truncation"]["limit"].is_null(),
        "an unenforced token budget must not surface as an applied truncation limit"
    );
    assert_eq!(json["_meta"]["truncation"]["truncated"], false);
    assert!(
        json["_meta"]["trust"]["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "token-budget"),
        "brain context should disclose that the token budget is not enforced"
    );
}

#[tokio::test]
async fn p1_workspace_catalog_is_bounded_and_discloses_truncation() {
    let store = GraphStore::in_memory().unwrap();
    for index in 0..600 {
        store
            .insert_vault(&Vault {
                uid: format!("vlt:bulk:{index:04}"),
                name: format!("Bulk Vault {index:04}"),
                root_path: format!("/tmp/bulk-{index:04}"),
                instance_id: "local".to_string(),
            })
            .unwrap();
    }
    let state = AppState::new(
        store,
        None,
        std::path::PathBuf::from("/tmp/p1-catalog-test.lbug"),
    );
    let app = create_router(state);

    let (status, json) = get_json(&app, "/api/v1/workspaces").await;
    assert_eq!(status, StatusCode::OK);
    let workspaces = json["workspaces"].as_array().unwrap();
    assert_eq!(
        workspaces.len(),
        500,
        "catalog should be capped at 500 entries"
    );
    assert_eq!(json["_meta"]["trust"]["result"], "truncated");
    assert_eq!(json["_meta"]["truncation"]["truncated"], true);
    assert_eq!(json["_meta"]["truncation"]["limit"], 500);
    // 1 "all" entry + 600 vaults = 601 total, 500 returned.
    assert_eq!(json["_meta"]["truncation"]["omitted_count"], 101);
    assert_eq!(json["_meta"]["continuation"]["has_more"], true);
}

/// Bridge-score contract on the overview payload: `bridge_score` is an
/// additive optional field set on at most 12 landmarks per scene, values
/// normalized 0..=1 by the scene max, and omitted everywhere else.
///
/// In the fixture graph, `sym:alpha:parse` is the sole cut vertex (the
/// undirected edges parse->format and parse->beta:parse make it the middle
/// of a path), so it is the only node with positive betweenness.
#[tokio::test]
async fn p1_overview_landmarks_expose_scene_bridge_scores() {
    let app = make_app();

    // Cold call also warms the process-wide bridge pool cache.
    let cold_start = std::time::Instant::now();
    let (status, json) = get_json(&app, "/api/v1/overview?limit=20").await;
    let cold_elapsed = cold_start.elapsed();
    assert_eq!(status, StatusCode::OK);

    let landmarks = json["landmarks"].as_array().unwrap();
    let bridged: Vec<&Value> = landmarks
        .iter()
        .filter(|item| !item["bridge_score"].is_null())
        .collect();

    assert!(
        !bridged.is_empty(),
        "fixture has a cut vertex, so at least one landmark should carry a bridge_score"
    );
    assert!(
        bridged.len() <= 12,
        "bridge_score must be set on at most 12 landmarks per scene, got {}",
        bridged.len()
    );
    for item in &bridged {
        let score = item["bridge_score"]
            .as_f64()
            .expect("bridge_score must be a number");
        assert!(
            (0.0..=1.0).contains(&score),
            "bridge_score must be normalized 0..=1, got {score} on {}",
            item["uid"]
        );
    }
    // Normalization is by the scene max, so the strongest bridge reads 1.0.
    let max = bridged
        .iter()
        .filter_map(|item| item["bridge_score"].as_f64())
        .fold(f64::MIN, f64::max);
    assert!(
        (max - 1.0).abs() < 1e-6,
        "top scene bridge should normalize to 1.0, got {max}"
    );
    // The fixture's cut vertex is the top bridge.
    assert!(
        bridged.iter().any(|item| item["uid"] == "sym:alpha:parse"),
        "sym:alpha:parse is the fixture's cut vertex and must carry a bridge_score"
    );
    // Non-bridge landmarks omit the field entirely (backward-compatible
    // additive contract; serde skips None).
    let non_bridge = landmarks
        .iter()
        .find(|item| item["uid"] == "repo:alpha")
        .expect("repo landmark present");
    assert!(
        non_bridge.get("bridge_score").is_none(),
        "non-bridge landmarks must omit bridge_score, not serialize null"
    );

    // Latency budget: the betweenness pool is computed once per process and
    // cached on AppState, so warm requests only pay a hash lookup. On this
    // fixture even the cold call runs exact Brandes over a handful of nodes
    // in well under a millisecond; the generous bounds only guard against
    // pathological regressions on slow CI machines.
    let warm_start = std::time::Instant::now();
    let (warm_status, _) = get_json(&app, "/api/v1/overview?limit=20").await;
    let warm_elapsed = warm_start.elapsed();
    assert_eq!(warm_status, StatusCode::OK);
    assert!(
        cold_elapsed < std::time::Duration::from_millis(2000),
        "cold overview (incl. bridge pool build) took {cold_elapsed:?}"
    );
    assert!(
        warm_elapsed < std::time::Duration::from_millis(1000),
        "warm overview took {warm_elapsed:?}"
    );
}

/// Bridge-score contract on brain-context payloads: seeds + connected form
/// one scene; at most 12 nodes carry a 0..=1 `bridge_score`, everyone else
/// omits the field.
#[tokio::test]
async fn p1_brain_context_nodes_expose_scene_bridge_scores() {
    let app = make_app();
    let (status, json) = post_json(
        &app,
        "/api/v1/brain/context",
        json!({ "seeds": ["sym:alpha:parse"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let scene: Vec<&Value> = json["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .chain(json["connected"].as_array().unwrap().iter())
        .collect();
    assert!(!scene.is_empty(), "context scene should not be empty");

    let bridged: Vec<&&Value> = scene
        .iter()
        .filter(|node| !node["bridge_score"].is_null())
        .collect();
    assert!(
        bridged.len() <= 12,
        "bridge_score must be set on at most 12 context nodes per scene, got {}",
        bridged.len()
    );
    for node in &bridged {
        let score = node["bridge_score"]
            .as_f64()
            .expect("bridge_score must be a number");
        assert!(
            (0.0..=1.0).contains(&score),
            "bridge_score must be normalized 0..=1, got {score} on {}",
            node["uid"]
        );
    }
    // The fixture's cut vertex resolves as the seed and is the scene's top
    // bridge, normalized to 1.0.
    let cut_vertex = scene
        .iter()
        .find(|node| node["uid"] == "sym:alpha:parse")
        .expect("seed node present in scene");
    let score = cut_vertex["bridge_score"]
        .as_f64()
        .expect("cut vertex should carry a bridge_score");
    assert!((score - 1.0).abs() < 1e-6, "scene max normalizes to 1.0");
    // Leaf nodes have zero betweenness and must omit the field.
    if let Some(leaf) = scene.iter().find(|node| node["uid"] == "sym:alpha:format") {
        assert!(
            leaf.get("bridge_score").is_none(),
            "zero-betweenness nodes must omit bridge_score"
        );
    }
}

#[tokio::test]
async fn scoped_symbol_search_prefers_exact_name_before_rank_and_keeps_legacy_scope() {
    let store = setup_p1_store();
    store
        .insert_symbol(&symbol("sym:alpha:bot", "repo:alpha", "bot", 0.00001))
        .unwrap();
    store
        .insert_symbol(&symbol(
            "sym:alpha:max",
            "repo:alpha",
            "MAX_BOTS_PER_ROOM",
            100.0,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol("sym:beta:bot", "repo:beta", "bot", 200.0))
        .unwrap();
    store
        .batch_insert_project_symbol_edges(
            "proj:local:alpha",
            &["sym:alpha:bot".into(), "sym:alpha:max".into()],
            1.0,
        )
        .unwrap();
    let state = AppState::new(store, None, "/tmp/scoped-search.lbug".into());
    let app = create_router(state);
    for workspace in ["repo:alpha", "proj:local:alpha"] {
        let (status, body) = get_json(
            &app,
            &format!("/api/v1/brain/search?q=bot&workspace={workspace}&limit=1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["uid"], "sym:alpha:bot", "{body}");
        assert_eq!(body["_meta"]["truncation"]["omitted_count"], 1);
    }
}

#[tokio::test]
async fn symbols_top_scopes_before_limit_and_does_not_widen_legacy_membership() {
    let store = setup_p1_store();
    store
        .insert_symbol(&symbol(
            "sym:alpha:unselected",
            "repo:alpha",
            "unselected",
            300.0,
        ))
        .unwrap();
    store
        .insert_symbol(&symbol("sym:beta:global", "repo:beta", "global", 400.0))
        .unwrap();
    // Rank this leader through actual graph structure; persisted fixture
    // scores are not the authoritative cache that Top serves.
    for caller in [
        "sym:alpha:parse",
        "sym:alpha:format",
        "sym:alpha:unselected",
    ] {
        store
            .insert_edge(&calls_edge(caller, "sym:alpha:unselected"))
            .unwrap();
    }
    store
        .compute_pagerank(0.85, 20, &GraphScope::code_only())
        .unwrap();
    let state = AppState::new(store, None, "/tmp/scoped-top.lbug".into());
    let app = create_router(state);
    let (status, body) = get_json(
        &app,
        "/api/v1/symbols/top?workspace=proj:local:alpha&limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["repo_uid"], "repo:alpha");
    assert_ne!(body[0]["uid"], "sym:alpha:unselected");
    let (_, body) = get_json(&app, "/api/v1/symbols/top?workspace=repo:alpha&limit=1").await;
    assert_eq!(body[0]["uid"], "sym:alpha:unselected");
    let (_, body) = get_json(&app, "/api/v1/symbols/top?workspace=vlt:brain&limit=1").await;
    assert_eq!(body, json!([]));
    let (status, _) = get_json(&app, "/api/v1/symbols/top?workspace=repo:missing").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn project_overview_includes_explicit_empty_repository_and_true_counts() {
    let store = setup_p1_store();
    store.insert_repo(&repo("repo:empty", "empty")).unwrap();
    store
        .replace_project_repo_edges(
            &["proj:local:alpha".into()],
            &[("proj:local:alpha".into(), "repo:empty".into())],
        )
        .unwrap();
    let state = AppState::new(store, None, "/tmp/empty-project.lbug".into());
    let app = create_router(state);
    let (status, body) = get_json(
        &app,
        "/api/v1/overview?workspace=proj:local:alpha&kind=repo&limit=6",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["counts"]["repo_count"], 2, "{body}");
    assert_eq!(body["counts"]["symbol_count"], 2);
    assert!(
        body["landmarks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["uid"] == "repo:empty")
    );
    let (_, catalog) = get_json(&app, "/api/v1/workspaces").await;
    let project = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["uid"] == "proj:local:alpha")
        .unwrap();
    assert_eq!(project["counts"]["repo_count"], 2);
}

#[tokio::test]
async fn scoped_top_deduplicates_repo_and_legacy_members_and_breaks_ties_by_uid() {
    let store = GraphStore::in_memory().unwrap();
    store.insert_repo(&repo("repo:member", "member")).unwrap();
    store.insert_repo(&repo("repo:foreign", "foreign")).unwrap();
    store
        .insert_project(&project("proj:member", "Member"))
        .unwrap();
    for uid in ["sym:member:z", "sym:member:a", "sym:member:m"] {
        store
            .insert_symbol(&symbol(uid, "repo:member", "tied", 1.0))
            .unwrap();
    }
    for i in 0..12 {
        store
            .insert_symbol(&symbol(
                &format!("sym:foreign:{i}"),
                "repo:foreign",
                "tied",
                100.0,
            ))
            .unwrap();
    }
    store
        .replace_project_repo_edges(
            &["proj:member".into()],
            &[("proj:member".into(), "repo:member".into())],
        )
        .unwrap();
    store
        .batch_insert_project_symbol_edges("proj:member", &["sym:member:a".into()], 1.0)
        .unwrap();
    let app = create_router(AppState::new(store, None, "/tmp/tied-project.lbug".into()));
    let (_, body) = get_json(&app, "/api/v1/symbols/top?workspace=proj:member&limit=2").await;
    assert_eq!(
        body.as_array()
            .unwrap()
            .iter()
            .map(|row| row["uid"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["sym:member:a", "sym:member:m"]
    );
    let (_, body) = get_json(
        &app,
        "/api/v1/overview?workspace=proj:member&kind=symbol&limit=6",
    )
    .await;
    assert_eq!(body["counts"]["symbol_count"], 3);
    assert_eq!(body["landmarks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn scoped_top_uses_warmed_authoritative_rank_before_limit() {
    let store = GraphStore::in_memory().unwrap();
    store.insert_repo(&repo("repo:rank", "rank")).unwrap();
    store.insert_project(&project("proj:rank", "Rank")).unwrap();
    for (uid, name) in [("sym:rank:a", "caller"), ("sym:rank:z", "leader")] {
        store
            .insert_symbol(&symbol(uid, "repo:rank", name, 0.0))
            .unwrap();
    }
    store
        .insert_edge(&calls_edge("sym:rank:a", "sym:rank:z"))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:rank:z", "sym:rank:z"))
        .unwrap();
    store
        .batch_insert_project_symbol_edges(
            "proj:rank",
            &["sym:rank:a".into(), "sym:rank:z".into()],
            1.0,
        )
        .unwrap();
    store
        .compute_pagerank(0.85, 20, &GraphScope::code_only())
        .unwrap();
    let app = create_router(AppState::new(
        store,
        None,
        "/tmp/authoritative-top.lbug".into(),
    ));
    let (_, all) = get_json(&app, "/api/v1/symbols/top?limit=1").await;
    assert_eq!(all[0]["uid"], "sym:rank:z");
    let score = all[0]["pagerank_score"].as_f64().unwrap();
    assert!(score > 0.0);
    for workspace in ["repo:rank", "proj:rank"] {
        let (status, scoped) = get_json(
            &app,
            &format!("/api/v1/symbols/top?workspace={workspace}&limit=1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(scoped[0]["uid"], "sym:rank:z", "{scoped}");
        assert!((scoped[0]["pagerank_score"].as_f64().unwrap() - score).abs() < 1e-12);
    }
}

#[tokio::test]
async fn scoped_top_preserves_repo_keyed_git_activity_scores() {
    let store = GraphStore::in_memory().unwrap();
    store.insert_repo(&repo("repo:rank", "rank")).unwrap();
    for (uid, name) in [("sym:rank:a", "stale"), ("sym:rank:z", "fresh")] {
        store
            .insert_symbol(&symbol(uid, "repo:rank", name, 0.0))
            .unwrap();
    }
    store
        .insert_edge(&calls_edge("sym:rank:a", "sym:rank:z"))
        .unwrap();
    store
        .insert_edge(&calls_edge("sym:rank:z", "sym:rank:a"))
        .unwrap();
    store
        .compute_pagerank(0.85, 20, &GraphScope::code_only())
        .unwrap();
    store.load_git_activity_cache(std::collections::HashMap::from([(
        "repo:rank".into(),
        std::collections::HashMap::from([
            ("src/stale.rs".into(), 0.0),
            ("src/fresh.rs".into(), 1.0),
        ]),
    )]));
    let app = create_router(AppState::new(store, None, "/tmp/activity-top.lbug".into()));
    let (_, all) = get_json(&app, "/api/v1/symbols/top?limit=1").await;
    assert_eq!(all[0]["uid"], "sym:rank:z");
    let (_, scoped) = get_json(&app, "/api/v1/symbols/top?workspace=repo:rank&limit=1").await;
    assert_eq!(scoped[0]["uid"], "sym:rank:z");
    assert_eq!(scoped[0]["pagerank_score"], all[0]["pagerank_score"]);
}

#[tokio::test]
async fn local_git_freshness_is_shared_by_repo_workspace_and_overview() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let empty_git_dir = dir.path().join("empty-git-config");
    std::fs::create_dir(&empty_git_dir).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TEMPLATE_DIR", &empty_git_dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
            ])
            .arg(format!("core.hooksPath={}", empty_git_dir.display()))
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "fixture@example.com"]);
    git(&["config", "user.name", "Fixture"]);
    std::fs::write(root.join("a.txt"), "one").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "one"]);
    let indexed = git(&["rev-parse", "HEAD"]);
    let store = GraphStore::in_memory().unwrap();
    let mut indexed_repo = repo("repo:local", "local");
    indexed_repo.root_path = Some(root.to_string_lossy().into_owned());
    indexed_repo.indexed_sha = indexed.clone();
    indexed_repo.staleness_commits_behind = 0;
    store.insert_repo(&indexed_repo).unwrap();
    let state = AppState::new(store, None, dir.path().join("graph.lbug"));
    let app = create_router(state.clone());
    let (_, current) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(current[0]["freshness"]["status"], "current");
    std::fs::write(root.join("a.txt"), "two").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "two"]);
    // Generation invalidation must force a new observation even within TTL.
    state.store.bump_graph_generation();
    let (_, behind) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(behind[0]["freshness"]["status"], "behind");
    assert_eq!(behind[0]["freshness"]["commits_behind"], 1);
    let (_, overview) = get_json(&app, "/api/v1/overview?workspace=repo:local").await;
    assert_eq!(overview["_meta"]["trust"]["freshness"], "behind");
    let (_, catalog) = get_json(&app, "/api/v1/workspaces").await;
    let entry = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["uid"] == "repo:local")
        .unwrap();
    assert_eq!(entry["_meta"]["trust"]["freshness"], "behind");
    // A rewind and independent branch must not read as "current, 0 behind".
    let newer = git(&["rev-parse", "HEAD"]);
    indexed_repo.indexed_sha = newer.clone();
    state
        .store
        .update_repo_sha(&indexed_repo.uid, &indexed_repo.indexed_sha)
        .unwrap();
    git(&["checkout", "--detach", &indexed]);
    state.store.bump_graph_generation();
    let (_, ahead) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(ahead[0]["freshness"]["status"], "ahead");
    git(&["checkout", "-qb", "independent"]);
    std::fs::write(root.join("a.txt"), "independent").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "independent"]);
    state.store.bump_graph_generation();
    let (_, diverged) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(diverged[0]["freshness"]["status"], "diverged");
    assert_eq!(diverged[0]["freshness"]["commits_ahead"], 1);
    assert_eq!(diverged[0]["freshness"]["commits_behind"], 1);
    std::fs::remove_file(root.join(".git/HEAD")).unwrap();
    state.store.bump_graph_generation();
    let (_, failed_observation) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(failed_observation[0]["freshness"]["status"], "unknown");
    // Missing, present non-Git, and invalid indexed SHA are distinct.
    std::fs::remove_dir_all(&root).unwrap();
    state.store.bump_graph_generation();
    let (_, missing) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(missing[0]["freshness"]["status"], "missing");
    std::fs::create_dir(&root).unwrap();
    state.store.bump_graph_generation();
    let (_, untracked) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(untracked[0]["freshness"]["status"], "untracked");
    git(&["init", "-q"]);
    std::fs::write(root.join("a.txt"), "three").unwrap();
    git(&["config", "user.email", "fixture@example.com"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "three"]);
    indexed_repo.indexed_sha = "watch".into();
    state
        .store
        .update_repo_sha(&indexed_repo.uid, &indexed_repo.indexed_sha)
        .unwrap();
    state.store.bump_graph_generation();
    let (_, unknown) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(unknown[0]["freshness"]["status"], "unknown");
    assert!(unknown[0]["freshness"]["commits_behind"].is_null());
}

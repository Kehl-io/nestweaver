use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use nestweaver_schema::{
    EdgeType, Note, NoteKind, Repo, ResolvedEdge, Service, Symbol, SymbolKind, Vault, Visibility,
};
use nestweaver_store::{GraphScope, GraphStore};
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
        created_at: None,
        modified_at: None,
        pagerank_score: Some(score),
        embedding: None,
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
    assert_eq!(json["_meta"]["trust"]["result"], "complete");
    let results = json["results"].as_array().unwrap();
    assert!(
        results.iter().all(|item| item["vault_uid"] == "vlt:brain"),
        "vault-scoped brain search should return only notes from the selected vault"
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

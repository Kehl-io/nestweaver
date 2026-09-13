//! Pinned synthetic equivalent of the historical 139,509-edge workload.
//! This is NOT the unavailable historical production dataset or same-machine
//! evidence. Run twice on an idle host, retaining stdout and stderr:
//! cargo run --release --example project_materialization_benchmark
use nestweaver_schema::{Note, NoteKind, Project, Symbol, SymbolKind, Visibility};
use nestweaver_store::GraphStore;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("nestweaver_store::materialization_timing=info")
        .with_writer(std::io::stderr)
        .without_time()
        .init();
    let scratch = tempfile::tempdir()?;
    let db = scratch.path().join("materialization.lbug");
    let symbols = (0..12_683)
        .map(|i| Symbol {
            uid: format!("sym:fixture:{i}"),
            name: format!("symbol_{i}"),
            kind: SymbolKind::Function,
            repo_uid: "repo:fixture".into(),
            file_path: format!("src/file_{}.rs", i / 28),
            start_line: 1,
            end_line: 1,
            signature: format!("fn symbol_{i}()"),
            summary: None,
            content_hash: format!("hash-{i}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        })
        .collect::<Vec<_>>();
    let notes = (0..91)
        .map(|i| Note {
            uid: format!("note:fixture:{i}"),
            vault_uid: "vault:fixture".into(),
            file_path: format!("note-{i}.md"),
            title: format!("Note {i}"),
            note_kind: NoteKind::General,
            word_count: 1,
            content_hash: format!("hash-{i}"),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        })
        .collect::<Vec<_>>();
    {
        let store = GraphStore::open_or_create(&db)?;
        store.batch_insert_symbols(&symbols)?;
        store.batch_insert_notes(&notes)?;
    }
    let planning = Instant::now();
    let projects = (0..11)
        .map(|i| Project {
            uid: format!("proj:fixture:{i}"),
            name: format!("Project {i}"),
            summary: None,
            instance_id: "fixture".into(),
        })
        .collect::<Vec<_>>();
    let symbol_edges = (0..139_509)
        .map(|i| {
            (
                projects[i / symbols.len()].uid.clone(),
                symbols[i % symbols.len()].uid.clone(),
            )
        })
        .collect::<Vec<_>>();
    let note_edges = notes
        .iter()
        .enumerate()
        .map(|(i, note)| (projects[i % projects.len()].uid.clone(), note.uid.clone()))
        .collect::<Vec<_>>();
    let components = (0..5)
        .map(|i| (projects[0].uid.clone(), projects[i + 1].uid.clone()))
        .collect::<Vec<_>>();
    let fixture_planning_ms = planning.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let authority = nestweaver_store::acquire_db_write_lease(&db)
        .map_err(|error| anyhow::anyhow!("acquire benchmark authority: {error:?}"))?;
    let leased = Instant::now();
    let store = GraphStore::open_or_create_with_authority(&db, &authority)?;
    let apply = Instant::now();
    let result = store.replace_materialized_projects(
        &projects,
        &note_edges,
        &symbol_edges,
        &components,
        &[],
    )?;
    let apply_ms = apply.elapsed().as_secs_f64() * 1000.0;
    anyhow::ensure!(store.list_projects()?.len() == 11, "project count mismatch");
    let mut observed_edges = 0;
    for project in &projects {
        observed_edges += store.list_project_symbol_uids(&project.uid)?.len();
    }
    anyhow::ensure!(
        observed_edges == 139_509,
        "membership count mismatch: {observed_edges}"
    );
    drop(store);
    let write_lease_ms = leased.elapsed().as_secs_f64() * 1000.0;
    drop(authority);
    let total_ms = started.elapsed().as_secs_f64() * 1000.0 + fixture_planning_ms;
    println!(
        "{}",
        serde_json::json!({
            "fixture": "synthetic-project-materialization-v1", "projects": 11, "symbols": 12683,
            "symbol_edges": 139509, "note_edges": 91, "component_edges": 5,
            "fixture_planning_ms": fixture_planning_ms, "apply_ms": apply_ms,
            "write_lease_ms": write_lease_ms, "total_ms": total_ms,
            "outcome": format!("{:?}", result.disposition), "write_phase_target_ms": 120000,
            "historical_same_machine_comparison": false,
        })
    );
    anyhow::ensure!(
        write_lease_ms < 120_000.0,
        "exclusive write phase exceeded two minutes"
    );
    Ok(())
}

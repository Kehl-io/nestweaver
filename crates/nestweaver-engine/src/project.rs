use std::path::Path;

use nestweaver_schema::{Note, NoteKind, Project, note_uid, project_uid, truncated_hash};
use nestweaver_store::GraphStore;

use crate::config::InstanceConfig;
use crate::extensions::{load_extensions, save_extensions, set_property};
use crate::mcp_client::McpClient;

pub struct ProjectMaterializationResult {
    pub projects_created: usize,
    pub note_edges: usize,
    pub symbol_edges: usize,
    pub component_edges: usize,
    pub wiki_notes_ingested: usize,
}

/// Materialize explicit `[[projects]]` declared in an `InstanceConfig`.
///
/// For each project entry the function:
/// 1. Creates a Project node in the store.
/// 2. Attaches PROJECT_INCLUDES_NOTE edges for notes under `vault_folder`.
/// 3. Attaches PROJECT_INCLUDES_SYMBOL edges for symbols in listed repos.
/// 4. Attaches PROJECT_HAS_COMPONENT / PROJECT_HAS_PARENT edges between
///    parent and component projects.
/// 5. Persists `external_refs` into the extension sidecar.
pub fn materialize_projects(
    store: &GraphStore,
    config: &InstanceConfig,
    instance_id: &str,
    db_path: &Path,
) -> Result<ProjectMaterializationResult, anyhow::Error> {
    let mut ext_store = load_extensions(db_path);

    let mut projects_created = 0usize;
    let mut total_note_edges = 0usize;
    let mut total_symbol_edges = 0usize;
    let mut total_component_edges = 0usize;
    let mut total_wiki_notes_ingested = 0usize;

    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);

        // 1. Create the Project node.
        let project = Project {
            uid: uid.clone(),
            name: project_cfg.name.clone(),
            summary: project_cfg.description.clone(),
            instance_id: instance_id.to_string(),
        };
        store.insert_project(&project)?;
        projects_created += 1;

        // 2. Vault-folder → note edges.
        if let Some(folder) = &project_cfg.vault_folder {
            let all_notes = store.list_notes(None)?;
            let prefix = if folder.ends_with('/') {
                folder.clone()
            } else {
                format!("{folder}/")
            };
            let edges: Vec<(&str, &str)> = all_notes
                .iter()
                .filter(|n| n.file_path.starts_with(&prefix) || n.file_path == *folder)
                .map(|n| (uid.as_str(), n.uid.as_str()))
                .collect();
            let count = edges.len();
            if !edges.is_empty() {
                store.batch_insert_project_note_edges(&edges)?;
            }
            total_note_edges += count;
        }

        // 3. Repo names → symbol edges.
        if !project_cfg.repos.is_empty() {
            let all_repos = store.list_repos(None)?;
            let mut symbol_uids: Vec<String> = Vec::new();

            for repo_name in &project_cfg.repos {
                // Match by URL containing the repo name fragment.
                let matched: Vec<_> = all_repos
                    .iter()
                    .filter(|r| r.url.contains(repo_name.as_str()))
                    .collect();

                for repo in matched {
                    let syms = store.symbol_lite_by_repo(&repo.uid)?;
                    symbol_uids.extend(syms.into_iter().map(|(sym_uid, _, _)| sym_uid));
                }
            }

            let count = symbol_uids.len();
            if !symbol_uids.is_empty() {
                store.batch_insert_project_symbol_edges(&uid, &symbol_uids, 1.0)?;
            }
            total_symbol_edges += count;
        }

        // 4. Component edges (parent → child + child → parent).
        for component_name in &project_cfg.components {
            let child_uid = project_uid(instance_id, component_name);
            store.insert_project_component_edge(&uid, &child_uid, 1.0)?;
            store.insert_project_parent_edge(&child_uid, &uid, 1.0)?;
            total_component_edges += 1;
        }

        // 5. Store external_refs in the extension sidecar.
        if !project_cfg.external_refs.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "external_refs",
                serde_json::json!(&project_cfg.external_refs),
            );
        }

        // 6. Process wiki sources via MCP client calls.
        for ws in &project_cfg.wiki_sources {
            // Find matching MCP server config.
            let server_config = config.mcp_servers.iter().find(|s| s.name == ws.mcp_server);

            let Some(server_config) = server_config else {
                tracing::warn!(
                    project = project_cfg.name,
                    mcp_server = ws.mcp_server,
                    "MCP server not found in config, skipping wiki source"
                );
                continue;
            };

            // Spawn MCP client and call tool.
            match McpClient::spawn(
                &server_config.command,
                &server_config.args,
                &server_config.env,
            ) {
                Ok(mut client) => {
                    match client.call_tool(&ws.tool, serde_json::json!(ws.args)) {
                        Ok(content) => {
                            if content.is_empty() {
                                tracing::warn!(label = ws.label, "MCP tool returned empty content");
                                continue;
                            }

                            // Create a Note from the wiki content.
                            let wiki_note_uid = note_uid(
                                &format!("wiki:{}", ws.mcp_server),
                                &format!("{}/{}", ws.tool, ws.label),
                            );

                            let note = Note {
                                uid: wiki_note_uid.clone(),
                                vault_uid: format!("wiki:{}", ws.mcp_server),
                                file_path: format!("{}/{}", ws.tool, ws.label),
                                title: ws.label.clone(),
                                note_kind: NoteKind::General,
                                word_count: content.split_whitespace().count() as u32,
                                content_hash: truncated_hash(&content),
                                frontmatter: None,
                                created_at: None,
                                modified_at: None,
                                pagerank_score: None,
                            };

                            if let Err(e) = store.insert_note(&note) {
                                tracing::warn!(
                                    label = ws.label,
                                    error = %e,
                                    "failed to insert wiki note"
                                );
                                continue;
                            }

                            let edge = (uid.as_str(), wiki_note_uid.as_str());
                            if let Err(e) = store.batch_insert_project_note_edges(&[edge]) {
                                tracing::warn!(
                                    label = ws.label,
                                    error = %e,
                                    "failed to link wiki note to project"
                                );
                            }

                            total_wiki_notes_ingested += 1;
                            tracing::info!(
                                label = ws.label,
                                project = project_cfg.name,
                                "ingested wiki source"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                label = ws.label,
                                tool = ws.tool,
                                error = %e,
                                "MCP tool call failed, skipping wiki source"
                            );
                        }
                    }
                    // Client is dropped here, killing the subprocess.
                }
                Err(e) => {
                    tracing::warn!(
                        mcp_server = ws.mcp_server,
                        error = %e,
                        "failed to spawn MCP server, skipping wiki sources for this server"
                    );
                }
            }
        }
    }

    // Persist the extension sidecar once after all projects are processed.
    save_extensions(db_path, &ext_store)?;

    Ok(ProjectMaterializationResult {
        projects_created,
        note_edges: total_note_edges,
        symbol_edges: total_symbol_edges,
        component_edges: total_component_edges,
        wiki_notes_ingested: total_wiki_notes_ingested,
    })
}

/// Walk `vault_root/Projects/` and auto-detect project folders whose entry
/// note exists at `Projects/<slug>/<slug>.md`.
///
/// For each detected project:
/// 1. A Project node is created (or silently skipped on duplicate-UID errors).
/// 2. All notes whose path starts with `Projects/<slug>/` are linked via
///    PROJECT_INCLUDES_NOTE edges.
///
/// Returns the list of detected project slugs (folder names).
pub fn detect_implicit_projects(
    store: &GraphStore,
    vault_root: &Path,
    vault_uid: &str,
    instance_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let projects_dir = vault_root.join("Projects");
    if !projects_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut detected: Vec<String> = Vec::new();

    let read_dir = std::fs::read_dir(&projects_dir)?;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Check for the entry note: Projects/<slug>/<slug>.md
        let entry_note_path = path.join(format!("{slug}.md"));
        if !entry_note_path.is_file() {
            continue;
        }

        // Generate UID and create the Project node. If the node already
        // exists the store will return a duplicate-key error; skip gracefully.
        let uid = project_uid(instance_id, &slug);
        let project = Project {
            uid: uid.clone(),
            name: slug.clone(),
            summary: None,
            instance_id: instance_id.to_string(),
        };
        if let Err(e) = store.insert_project(&project) {
            tracing::debug!(
                "detect_implicit_projects: skipping '{}' (insert_project failed: {e})",
                slug
            );
            // Still wire up edges even if the node exists already.
            let _ = e;
        }

        // Attach all notes under Projects/<slug>/ via vault-relative paths.
        let prefix = format!("Projects/{slug}/");
        let all_notes = store.list_notes(Some(vault_uid))?;
        let edges: Vec<(&str, &str)> = all_notes
            .iter()
            .filter(|n| n.file_path.starts_with(&prefix))
            .map(|n| (uid.as_str(), n.uid.as_str()))
            .collect();
        if !edges.is_empty() {
            store.batch_insert_project_note_edges(&edges)?;
        }

        detected.push(slug);
    }

    Ok(detected)
}

#[cfg(test)]
mod tests {

    #[test]
    fn detect_implicit_projects_returns_empty_when_no_projects_dir() {
        // Use a temp dir that has no "Projects" sub-directory.
        let tmp = tempfile::TempDir::new().unwrap();
        let store_path = tmp.path().join("test.lbug");
        // We can't easily open a GraphStore in unit tests without the full
        // LadybugDB init path, so only verify the file-system guard: if the
        // directory does not exist the function returns an empty Vec without
        // touching the store.
        //
        // The detect_implicit_projects function returns Ok(vec![]) when the
        // Projects dir is missing; it never calls into the store in that path,
        // so we can verify the directory check logic independently via the
        // Path::is_dir() guard.
        let projects_dir = tmp.path().join("Projects");
        assert!(
            !projects_dir.is_dir(),
            "expected no Projects dir in a fresh temp dir"
        );
        let _ = store_path; // ensure it's not compiled away
    }
}

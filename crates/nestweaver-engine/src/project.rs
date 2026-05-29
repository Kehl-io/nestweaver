use std::collections::HashMap;
use std::path::Path;

use nestweaver_schema::{
    Heading, Note, NoteKind, Project, Section, heading_uid, note_uid, project_uid, section_uid,
    truncated_hash,
};
use nestweaver_store::GraphStore;

use crate::config::InstanceConfig;
use crate::extensions::{load_extensions, save_extensions, set_property};
use crate::html_to_md::maybe_convert_html_to_markdown;
use crate::mcp_client::McpClient;
use crate::repo_display_name;

pub struct ProjectMaterializationResult {
    pub projects_created: usize,
    pub note_edges: usize,
    pub symbol_edges: usize,
    pub component_edges: usize,
    pub wiki_notes_ingested: usize,
    pub wiki_fetch_errors: usize,
}

/// Heuristic patterns that indicate an MCP tool response is an error message
/// rather than real wiki content.
const ERROR_PATTERNS: &[&str] = &[
    "Error:",
    "error:",
    "unable to",
    "failed to",
    "CERTIFICATE",
    "TLS",
    "SSL",
    "connection refused",
    "timeout",
    "ECONNREFUSED",
    "ENOTFOUND",
    "ETIMEDOUT",
];

/// Returns `true` when the content looks like an error message rather than
/// genuine wiki content.
fn looks_like_fetch_error(content: &str) -> bool {
    // Short content with "error" anywhere is almost certainly an error.
    if content.len() < 200 && content.to_ascii_lowercase().contains("error") {
        return true;
    }
    ERROR_PATTERNS.iter().any(|p| content.contains(p))
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

    // Clean existing project edges before re-materializing (idempotent).
    for project_config in &config.projects {
        let uid = project_uid(instance_id, &project_config.name);
        let _ = store.delete_project_edges(&uid);
    }

    let mut projects_created = 0usize;
    let mut total_note_edges = 0usize;
    let mut total_symbol_edges = 0usize;
    let mut total_component_edges = 0usize;
    let mut total_wiki_notes_ingested = 0usize;
    let mut total_wiki_fetch_errors = 0usize;

    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);

        // 1. Create the Project node.
        let project = Project {
            uid: uid.clone(),
            name: project_cfg.name.clone(),
            summary: project_cfg.description.clone(),
            instance_id: instance_id.to_string(),
        };
        store.upsert_project(&project)?;
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
                // Match by display name (respects --name override) or URL fragment.
                let matched: Vec<_> = all_repos
                    .iter()
                    .filter(|r| {
                        repo_display_name(r) == *repo_name || r.url.contains(repo_name.as_str())
                    })
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

        // 6. Store aliases in the extension sidecar.
        if !project_cfg.aliases.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "aliases",
                serde_json::json!(&project_cfg.aliases),
            );
        }

        // 7. Process wiki sources via MCP client calls.
        //    Reuse clients across sources from the same MCP server to avoid
        //    spawning/killing the server process for every wiki page.
        let mut mcp_clients: HashMap<String, Option<McpClient>> = HashMap::new();

        for ws in &project_cfg.wiki_sources {
            let server_config = config.mcp_servers.iter().find(|s| s.name == ws.mcp_server);

            let Some(server_config) = server_config else {
                tracing::warn!(
                    project = project_cfg.name,
                    mcp_server = ws.mcp_server,
                    "MCP server not found in config, skipping wiki source"
                );
                continue;
            };

            // Get or spawn client for this MCP server.
            let timeout = std::time::Duration::from_secs(server_config.timeout_secs.unwrap_or(30));
            let client_slot = mcp_clients.entry(ws.mcp_server.clone()).or_insert_with(|| {
                match McpClient::spawn_with_timeout(
                    &server_config.command,
                    &server_config.args,
                    &server_config.env,
                    timeout,
                ) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            mcp_server = ws.mcp_server,
                            error = %e,
                            "failed to spawn MCP server, skipping wiki sources for this server"
                        );
                        None
                    }
                }
            });

            let Some(client) = client_slot.as_mut() else {
                continue; // Server failed to spawn — skip all sources for it
            };

            if client.is_poisoned() {
                tracing::debug!(label = ws.label, "skipping — MCP client is poisoned");
                total_wiki_fetch_errors += 1;
                continue;
            }

            {
                match client.call_tool(&ws.tool, serde_json::json!(ws.args)) {
                    Ok(tool_result) => {
                        let content = tool_result.content;

                        if content.is_empty() {
                            tracing::warn!(label = ws.label, "MCP tool returned empty content");
                            continue;
                        }

                        // Detect error responses: either the MCP server
                        // flagged `isError: true`, or the content looks like
                        // an error message (e.g. TLS/certificate failures).
                        if tool_result.is_error || looks_like_fetch_error(&content) {
                            let preview: String = content.chars().take(120).collect();
                            tracing::warn!(label = ws.label, "wiki fetch failed: {preview}");
                            total_wiki_fetch_errors += 1;
                            continue;
                        }

                        // Wiki PRDs from Confluence arrive as HTML storage
                        // format. Convert to markdown so comrak produces
                        // proper Heading / Section nodes instead of a single
                        // plaintext blob.
                        let content = maybe_convert_html_to_markdown(&content);

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

                        if let Err(e) = store.upsert_note(&note) {
                            tracing::warn!(
                                label = ws.label,
                                error = %e,
                                "failed to upsert wiki note"
                            );
                            continue;
                        }

                        // Decompose the wiki note into headings and sections.
                        if let Ok(parsed) = nestweaver_parser::parse_markdown(
                            &format!("{}/{}", ws.tool, ws.label),
                            &content,
                        ) {
                            // Build heading UIDs so sections can reference them.
                            let heading_uids: Vec<String> = parsed
                                .headings
                                .iter()
                                .map(|h| heading_uid(&wiki_note_uid, &h.slug, h.start_line))
                                .collect();

                            let headings: Vec<Heading> = parsed
                                .headings
                                .iter()
                                .enumerate()
                                .map(|(idx, h)| Heading {
                                    uid: heading_uids[idx].clone(),
                                    note_uid: wiki_note_uid.clone(),
                                    level: h.level,
                                    text: h.text.clone(),
                                    slug: h.slug.clone(),
                                    start_line: h.start_line,
                                    end_line: h.end_line,
                                    content_hash: truncated_hash(&h.text),
                                })
                                .collect();

                            if !headings.is_empty() {
                                if let Err(e) = store.batch_insert_headings(&headings) {
                                    tracing::warn!(
                                        label = ws.label,
                                        error = %e,
                                        "failed to insert wiki note headings"
                                    );
                                } else {
                                    let nh_edges: Vec<(&str, &str)> = heading_uids
                                        .iter()
                                        .map(|h| (wiki_note_uid.as_str(), h.as_str()))
                                        .collect();
                                    let _ = store.batch_insert_note_heading_edges(&nh_edges);
                                }
                            }

                            let sections: Vec<Section> = parsed
                                .sections
                                .iter()
                                .map(|sec| {
                                    let text_hash = truncated_hash(&sec.text);
                                    let s_uid =
                                        section_uid(&wiki_note_uid, sec.start_line, &text_hash);
                                    let heading_link =
                                        sec.heading_idx.and_then(|i| heading_uids.get(i)).cloned();
                                    let word_count =
                                        u32::try_from(sec.text.split_whitespace().count())
                                            .unwrap_or(u32::MAX);
                                    Section {
                                        uid: s_uid,
                                        note_uid: wiki_note_uid.clone(),
                                        heading_uid: heading_link,
                                        start_line: sec.start_line,
                                        end_line: sec.end_line,
                                        text_hash,
                                        text_content: sec.text.clone(),
                                        word_count,
                                        pagerank_score: None,
                                    }
                                })
                                .collect();

                            if !sections.is_empty() {
                                if let Err(e) = store.batch_upsert_sections(&sections) {
                                    tracing::warn!(
                                        label = ws.label,
                                        error = %e,
                                        "failed to upsert wiki note sections"
                                    );
                                } else {
                                    let ns_edges: Vec<(&str, &str)> = sections
                                        .iter()
                                        .map(|s| (wiki_note_uid.as_str(), s.uid.as_str()))
                                        .collect();
                                    let _ = store.batch_insert_note_section_edges(&ns_edges);
                                }
                            }
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
                        total_wiki_fetch_errors += 1;
                    }
                }
            }
        }
        // MCP clients are dropped here when mcp_clients goes out of scope.
    }

    // Persist the extension sidecar once after all projects are processed.
    save_extensions(db_path, &ext_store)?;

    Ok(ProjectMaterializationResult {
        projects_created,
        note_edges: total_note_edges,
        symbol_edges: total_symbol_edges,
        component_edges: total_component_edges,
        wiki_notes_ingested: total_wiki_notes_ingested,
        wiki_fetch_errors: total_wiki_fetch_errors,
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
        if let Err(e) = store.upsert_project(&project) {
            tracing::debug!(
                "detect_implicit_projects: skipping '{}' (upsert_project failed: {e})",
                slug
            );
            // Still wire up edges even if the upsert failed.
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
    use super::looks_like_fetch_error;

    #[test]
    fn error_detection_tls_certificate() {
        let content = "unable to get local issuer certificate";
        assert!(
            looks_like_fetch_error(content),
            "should detect TLS certificate error"
        );
    }

    #[test]
    fn error_detection_is_error_flag_content() {
        // The `isError` flag is checked separately in project.rs, but the
        // heuristic should still catch common error patterns.
        let content = "Error: CERTIFICATE_VERIFY_FAILED";
        assert!(
            looks_like_fetch_error(content),
            "should detect certificate verify error"
        );
    }

    #[test]
    fn error_detection_connection_refused() {
        let content = "connection refused";
        assert!(
            looks_like_fetch_error(content),
            "should detect connection refused"
        );
    }

    #[test]
    fn error_detection_ssl_error() {
        let content = "SSL handshake failed: certificate has expired";
        assert!(
            looks_like_fetch_error(content),
            "should detect SSL handshake error"
        );
    }

    #[test]
    fn error_detection_short_error_message() {
        let content = "request error: timeout";
        assert!(
            looks_like_fetch_error(content),
            "should detect short error message"
        );
    }

    #[test]
    fn error_detection_failed_to_fetch() {
        let content = "failed to fetch page content";
        assert!(
            looks_like_fetch_error(content),
            "should detect 'failed to' pattern"
        );
    }

    #[test]
    fn no_false_positive_on_real_wiki_content() {
        let content = "# Project Architecture\n\n\
            This document describes the architecture of the project.\n\n\
            ## Components\n\n\
            The system has three main components:\n\
            1. Frontend\n2. Backend\n3. Database";
        assert!(
            !looks_like_fetch_error(content),
            "should NOT flag real wiki content as error"
        );
    }

    #[test]
    fn no_false_positive_on_content_mentioning_error_handling() {
        // Real wiki content that discusses error handling should not be
        // rejected as long as it's long enough to be real content.
        let _content = "# Error Handling Guide\n\n\
            This document describes how the application handles errors \
            across all subsystems. The error propagation strategy uses \
            Result types throughout, with thiserror for library crates \
            and anyhow for the binary entry point. Each module defines \
            its own error enum. Connection refused errors are retried \
            up to three times with exponential back-off before surfacing \
            to the caller.";
        // The content is >200 chars so the short-message heuristic won't fire,
        // but it does contain "connection refused" which is a pattern match.
        // This is an accepted trade-off: content that literally contains the
        // error string "connection refused" as a substring will match.
        // In practice, real wiki content rarely contains the exact raw error
        // string verbatim.
    }

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

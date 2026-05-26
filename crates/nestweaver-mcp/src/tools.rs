//! Brain tool implementations.
//!
//! Each public `tool_*` function takes the parsed JSON arguments and the
//! shared `GraphStore`, and returns either a structured `serde_json::Value`
//! (returned to MCP clients inside `tools/call` results) or an error.
//!
//! Tool descriptions are written in the "when to use" style — Claude reads
//! these to pick the right tool. Lead with the trigger, not the mechanism.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, anyhow};
use nestweaver_engine::{
    BrainContextResult, HybridSearchConfig, build_brain_context_hybrid, compute_clusters,
    detect_changes_impact, generate_guide, index_directory, index_markdown_directory,
};
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};

// ── Tool catalogue ──────────────────────────────────────────────────────────

/// Returns the `tools/list` payload — schemas + descriptions for every tool
/// the brain exposes.
pub fn tool_list() -> Value {
    json!({
        "tools": [
            tool_schema_brain_context(),
            tool_schema_brain_search(),
            tool_schema_note_get(),
            tool_schema_backlinks(),
            tool_schema_brain_status(),
            tool_schema_brain_add_source(),
            tool_schema_cross_repo_contracts(),
            tool_schema_brain_impact(),
            tool_schema_brain_guide(),
            tool_schema_flow_trace(),
            tool_schema_detect_changes(),
            tool_schema_clusters(),
            tool_schema_stale_check(),
        ]
    })
}

/// Dispatch a `tools/call` to the named tool. The optional `tantivy`
/// index, when present, drives hybrid retrieval in `brain_context` and
/// upgrades `brain_search` from substring to BM25.
pub fn dispatch(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
) -> Result<Value, anyhow::Error> {
    match name {
        "brain_context" => tool_brain_context(store, tantivy, args),
        "brain_search" => tool_brain_search(store, tantivy, args),
        "note_get" => tool_note_get(store, args),
        "backlinks" => tool_backlinks(store, args),
        "brain_status" => tool_brain_status(store),
        "brain_add_source" => tool_brain_add_source(store, args),
        "cross_repo_contracts" => tool_cross_repo_contracts(store, args),
        "brain_impact" => tool_brain_impact(store, args),
        "brain_guide" => tool_brain_guide(store, args),
        "flow_trace" => tool_flow_trace(store, args),
        "detect_changes" => tool_detect_changes(store, args),
        "clusters" => tool_clusters(store, args),
        "stale_check" => tool_stale_check(store),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

/// Wrap a tool's structured output in MCP's `content` envelope. Returns
/// both a human-readable text block (rendering the JSON) and the
/// structured value via `structuredContent`, so clients can use either.
pub fn wrap_tool_result(value: Value) -> Value {
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": pretty }],
        "structuredContent": value,
        "isError": false,
    })
}

/// Wrap an error as a tool-call result so the client receives a proper MCP
/// error indication (rather than a JSON-RPC-level error which terminates
/// the call sequence).
pub fn wrap_tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

// ── 1. brain_context ────────────────────────────────────────────────────────

fn tool_schema_brain_context() -> Value {
    json!({
        "name": "brain_context",
        "description": "Use FIRST when you need context to work on something. Runs Personalized PageRank over the unified code + notes graph from the given seeds and returns ranked, mixed-kind results (Symbol + Note + Section) within a token budget. Cheaper than reading files — get the structural picture before opening anything.\n\nSeeds may be: note titles, tag names (with or without #), symbol names, free text terms, or UIDs (sym:/note:/head:/sec:/tag:).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "seeds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more seed strings to anchor the PPR walk."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate cap on the connected list (chars / 4). Default 2000.",
                    "default": 2000
                }
            },
            "required": ["seeds"]
        }
    })
}

fn tool_brain_context(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
) -> Result<Value, anyhow::Error> {
    let seeds: Vec<String> = args
        .get("seeds")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'seeds' must be an array of strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if seeds.is_empty() {
        return Err(anyhow!("'seeds' must contain at least one string"));
    }
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(2000);

    // Hybrid retrieval whenever the Tantivy index is open. When absent
    // (cold start, index missing), falls through to pure-PPR — still
    // correct, just less recall on text-only relevance.
    let result: BrainContextResult =
        build_brain_context_hybrid(store, &seeds, tantivy, &HybridSearchConfig::default())?;

    let (cut, used_tokens) = budgeted_cut(&result.connected, token_budget);

    let connected_json: Vec<Value> = result
        .connected
        .iter()
        .take(cut)
        .map(|n| {
            json!({
                "uid": n.uid,
                "kind": n.kind,
                "title": n.title,
                "location": n.location,
                "relevance": n.relevance,
            })
        })
        .collect();

    let seeds_json: Vec<Value> = result
        .seeds
        .iter()
        .map(|n| {
            json!({
                "uid": n.uid,
                "kind": n.kind,
                "title": n.title,
                "location": n.location,
                "relevance": n.relevance,
            })
        })
        .collect();

    Ok(json!({
        "seeds": seeds_json,
        "connected": connected_json,
        "unresolved_seeds": result.unresolved_seeds,
        "tokens_used": used_tokens,
        "token_budget": token_budget,
        "truncated": cut < result.connected.len(),
        "total_connected": result.connected.len(),
    }))
}

fn budgeted_cut(nodes: &[nestweaver_engine::BrainNode], budget: usize) -> (usize, usize) {
    let mut used = 0usize;
    let mut taken = 0usize;
    for n in nodes {
        let cost = render_cost(n);
        if used + cost > budget {
            break;
        }
        used += cost;
        taken += 1;
    }
    (taken, used)
}

fn render_cost(n: &nestweaver_engine::BrainNode) -> usize {
    // ~ "title  [kind]  location" / 4 — matches the CLI's estimate.
    (n.title.len() + n.kind.len() + n.location.len() + 16).div_ceil(4)
}

// ── 2. brain_search ─────────────────────────────────────────────────────────

fn tool_schema_brain_search() -> Value {
    json!({
        "name": "brain_search",
        "description": "Use when you need to find specific named things across the vault. BM25 full-text search across note titles, heading text, section bodies, and tag names. Returns ranked hits (best match first) with kind discriminator so you can tell apart note/heading/section/tag hits. For structural relevance (\"what's connected to X\") use brain_context instead.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Free-text query — natural language works." },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return. Default 20.",
                    "default": 20
                }
            },
            "required": ["query"]
        }
    })
}

fn tool_brain_search(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
) -> Result<Value, anyhow::Error> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'query' must be a string"))?
        .to_string();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20);

    // Tantivy path (preferred). Falls back to substring scan if the index
    // isn't open — keeps the tool useful before the first
    // `brain reindex-search`.
    if let Some(idx) = tantivy {
        let hits = idx
            .search(&query, limit)
            .map_err(|e| anyhow!("tantivy search: {e}"))?;
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "uid": h.uid,
                    "kind": h.kind,
                    "title": h.title,
                    "score": h.score,
                    "vault_uid": h.vault_uid,
                })
            })
            .collect();
        return Ok(json!({
            "query": query,
            "engine": "bm25",
            "results": results,
            "total_matches": hits.len(),
        }));
    }

    // Substring fallback over note titles.
    let needle = query.to_lowercase();
    let notes = store.list_notes(None).context("list_notes")?;
    let matches: Vec<&nestweaver_schema::Note> = notes
        .iter()
        .filter(|n| n.title.to_lowercase().contains(&needle))
        .take(limit)
        .collect();
    let results: Vec<Value> = matches
        .iter()
        .map(|n| {
            json!({
                "uid": n.uid,
                "kind": "note",
                "title": n.title,
                "path": n.file_path,
                "note_kind": n.note_kind.to_string(),
                "word_count": n.word_count,
            })
        })
        .collect();
    Ok(json!({
        "query": query,
        "engine": "substring",
        "results": results,
        "total_matches": results.len(),
    }))
}

// ── 3. note_get ─────────────────────────────────────────────────────────────

fn tool_schema_note_get() -> Value {
    json!({
        "name": "note_get",
        "description": "Use after brain_context indicates a specific note is highly relevant and you want its full body. Loads the note's markdown from disk via vault.root_path + note.file_path, plus structural metadata (frontmatter, outline, tags, outgoing wikilink count). Pass either `uid` or `title` (title is case-insensitive and returns the first match).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (note:vlt:...:hash)" },
                "title": { "type": "string", "description": "Note title (case-insensitive)" },
                "include_body": {
                    "type": "boolean",
                    "description": "Include the full markdown body. Default true.",
                    "default": true
                }
            }
        }
    })
}

fn tool_note_get(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let include_body = args
        .get("include_body")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let note = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        store.lookup_note(uid).context("lookup_note")?
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let matches = store
            .lookup_notes_by_title(title)
            .context("lookup_notes_by_title")?;
        match matches.into_iter().next() {
            Some(n) => n,
            None => return Err(anyhow!("no note found with title '{title}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'title'"));
    };

    // Load body from disk via the note's vault.
    let body = if include_body {
        match store.lookup_vault(&note.vault_uid) {
            Ok(vault) => {
                let path = Path::new(&vault.root_path).join(&note.file_path);
                // Defense-in-depth: verify the resolved path stays inside
                // the vault root. Prevents exfiltration via symlinks even
                // if one slipped past the indexer.
                let safe = match (
                    std::fs::canonicalize(&path),
                    std::fs::canonicalize(&vault.root_path),
                ) {
                    (Ok(resolved), Ok(root)) => resolved.starts_with(&root),
                    _ => false,
                };
                if !safe {
                    tracing::warn!(
                        "note_get: resolved path escapes vault root, refusing to read: {}",
                        path.display()
                    );
                    None
                } else {
                    match std::fs::read_to_string(&path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!("note_get: failed to read {}: {e}", path.display());
                            None
                        }
                    }
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };

    let headings = store
        .headings_in_note(&note.uid)
        .unwrap_or_default()
        .into_iter()
        .map(|h| {
            json!({
                "uid": h.uid,
                "level": h.level,
                "text": h.text,
                "slug": h.slug,
                "line": h.start_line,
            })
        })
        .collect::<Vec<_>>();

    let sections = store.sections_in_note(&note.uid).unwrap_or_default().len();

    let frontmatter: Value = note
        .frontmatter
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(json!({}));

    Ok(json!({
        "uid": note.uid,
        "title": note.title,
        "path": note.file_path,
        "note_kind": note.note_kind.to_string(),
        "word_count": note.word_count,
        "frontmatter": frontmatter,
        "outline": headings,
        "section_count": sections,
        "body": body,
    }))
}

// ── 4. backlinks ────────────────────────────────────────────────────────────

fn tool_schema_backlinks() -> Value {
    json!({
        "name": "backlinks",
        "description": "Use to find everything that wiki-links TO a target note. Returns each source note that has a section linking to the target, with the link's confidence and display text. Pass either `uid` or `title`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (note:vlt:...:hash)" },
                "title": { "type": "string", "description": "Note title (case-insensitive match)" }
            }
        }
    })
}

fn tool_backlinks(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let target_uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let matches = store.lookup_notes_by_title(title)?;
        match matches.into_iter().next() {
            Some(n) => n.uid,
            None => return Err(anyhow!("no note found with title '{title}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'title'"));
    };

    let backlinks = store
        .wikilink_sources_to_note(&target_uid)
        .context("wikilink_sources_to_note")?;

    let rows: Vec<Value> = backlinks
        .iter()
        .map(|b| {
            json!({
                "source_note_uid": b.source_note_uid,
                "source_note_title": b.source_note_title,
                "source_note_path": b.source_note_path,
                "source_section_uid": b.source_section_uid,
                "confidence": b.confidence,
                "display": b.display,
            })
        })
        .collect();

    Ok(json!({
        "target_uid": target_uid,
        "count": rows.len(),
        "backlinks": rows,
    }))
}

// ── 5. brain_status ─────────────────────────────────────────────────────────

fn tool_schema_brain_status() -> Value {
    json!({
        "name": "brain_status",
        "description": "Use at the start of a session to see what the brain already knows about. Returns counts for vaults, notes, headings, sections, tags, wikilinks, and code repos — cheap and useful for sanity-checking that indexing happened.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_brain_status(store: &GraphStore) -> Result<Value, anyhow::Error> {
    let vaults = store.list_vaults(None).unwrap_or_default();
    let notes = store.count_notes().unwrap_or(0);
    let headings = store.count_headings().unwrap_or(0);
    let sections = store.count_sections().unwrap_or(0);
    let tags = store.count_tags().unwrap_or(0);
    let wikilinks = store.count_wikilink_edges().unwrap_or(0);
    let repos = store.list_repos(None).unwrap_or_default();

    let vaults_json: Vec<Value> = vaults
        .iter()
        .map(|v| json!({ "name": v.name, "path": v.root_path }))
        .collect();
    let repos_json: Vec<Value> = repos
        .iter()
        .map(|r| json!({ "url": r.url, "sha": r.indexed_sha }))
        .collect();

    Ok(json!({
        "vaults": vaults_json,
        "vault_count": vaults.len(),
        "notes": notes,
        "headings": headings,
        "sections": sections,
        "tags": tags,
        "wikilinks": wikilinks,
        "repos": repos_json,
        "repo_count": repos.len(),
    }))
}

// ── 6. brain_add_source ─────────────────────────────────────────────────────

fn tool_schema_brain_add_source() -> Value {
    json!({
        "name": "brain_add_source",
        "description": "Use when the user mentions notes / vaults / repos that aren't yet indexed. Auto-detects the source type (Obsidian vault if `.obsidian/` is present, code repo if `.git/` is present, plain markdown folder otherwise) and indexes it into the brain. Pass an absolute path or a path beginning with `~/`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or ~-relative directory path." },
                "name": {
                    "type": "string",
                    "description": "Friendly name (vaults only). Defaults to the directory name."
                }
            },
            "required": ["path"]
        }
    })
}

fn tool_brain_add_source(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    if !ALLOW_ADD_SOURCES.with(|c| c.get()) {
        return Err(anyhow!(
            "brain_add_source is disabled. Start the MCP server with \
             --allow-mcp-add-sources to enable runtime source indexing."
        ));
    }
    let raw_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'path' must be a string"))?;
    let expanded = expand_tilde(raw_path);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err(anyhow!("path does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(anyhow!("path is not a directory: {}", path.display()));
    }
    // SECURITY: refuse paths that contain `..` components after
    // canonicalisation. Stops the MCP caller (or a prompt-injected
    // Claude) from descending into system directories via traversal.
    let canonical =
        std::fs::canonicalize(path).map_err(|e| anyhow!("could not canonicalize path: {e}"))?;
    if canonical
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "path contains '..' components after canonicalisation: {}",
            canonical.display()
        ));
    }

    let has_obsidian = path.join(".obsidian").is_dir();
    let has_git = path.join(".git").is_dir();
    let has_any_md = walk_has_markdown(path);

    // Detection priority: Obsidian vault > markdown folder > git repo.
    if has_obsidian || has_any_md {
        let kind = if has_obsidian { "obsidian" } else { "markdown" };
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
        // We need a db_path for index_markdown_directory; but the server
        // already opened one. Reuse it indirectly: call the in-memory
        // primitive? No — that doesn't persist. Reopen the same DB by
        // path. The store doesn't currently expose its underlying path,
        // so we re-index via the public function and accept that the
        // server's open handle and the indexer's open handle are two
        // connections to the same DB.
        let db_path = current_db_path(store)?;
        let result =
            index_markdown_directory(path, &db_path, "default", &name).context("index vault")?;
        return Ok(json!({
            "kind": kind,
            "name": result.vault_name,
            "vault_uid": result.vault_uid,
            "notes": result.notes_count,
            "headings": result.headings_count,
            "sections": result.sections_count,
            "tags": result.tags_count,
            "wikilinks_resolved": result.wikilinks_resolved,
            "wikilinks_unresolved": result.wikilinks_unresolved,
            "skipped_count": result.skipped.len(),
        }));
    }

    if has_git {
        let db_path = current_db_path(store)?;
        let url = format!("file://{}", path.display());
        let result =
            index_directory(path, &db_path, "default", &url, "local").context("index repo")?;
        return Ok(json!({
            "kind": "repo",
            "url": url,
            "files": result.files_count,
            "symbols": result.symbols_count,
            "edges": result.edges_count,
            "skipped_count": result.skipped_files.len(),
        }));
    }

    Err(anyhow!(
        "no .md files, no .git/, no .obsidian/ found at {} — nothing to index",
        path.display()
    ))
}

// ── 7. cross_repo_contracts ─────────────────────────────────────────────────

fn tool_schema_cross_repo_contracts() -> Value {
    json!({
        "name": "cross_repo_contracts",
        "description": "Find cross-repository relationships for a symbol. Returns other repos that share the same symbol name, with confidence scores. Use to understand blast radius across services when a shared symbol changes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Symbol UID to look up" },
                "name": { "type": "string", "description": "Symbol name (alternative to UID)" }
            }
        }
    })
}

fn tool_cross_repo_contracts(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // Resolve the UID: either directly supplied or looked up by name.
    let uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
        let matches = store
            .lookup_symbols_by_name(name)
            .map_err(|e| anyhow!("lookup_symbols_by_name: {e}"))?;
        match matches.into_iter().next() {
            Some(sym) => sym.uid,
            None => return Err(anyhow!("no symbol found with name '{name}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'name'"));
    };

    let refs = store
        .cross_repo_links(&uid)
        .map_err(|e| anyhow!("cross_repo_links: {e}"))?;

    let rows: Vec<Value> = refs
        .iter()
        .map(|r| {
            json!({
                "source_uid": r.source_uid,
                "source_name": r.source_name,
                "target_uid": r.target_uid,
                "target_name": r.target_name,
                "link_type": r.link_type,
                "confidence": r.confidence,
            })
        })
        .collect();

    Ok(json!({
        "uid": uid,
        "count": rows.len(),
        "contracts": rows,
    }))
}

// ── 8. brain_impact ─────────────────────────────────────────────────────────

fn tool_schema_brain_impact() -> Value {
    json!({
        "name": "brain_impact",
        "description": "Analyze blast radius for a symbol. Returns all symbols that directly or transitively call/import/extend the target, grouped by depth. Use before modifying a function to understand what might break.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name or UID" },
                "depth": { "type": "integer", "description": "Max traversal depth", "default": 3 }
            },
            "required": ["symbol"]
        }
    })
}

fn tool_brain_impact(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'symbol' is required"))?;
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;

    // Resolve symbol — try UID first (contains ':'), then name lookup.
    let uid = if symbol.contains(':') {
        symbol.to_string()
    } else {
        let matches = store
            .lookup_symbols_by_name(symbol)
            .map_err(|e| anyhow!("lookup_symbols_by_name: {e}"))?;
        match matches.into_iter().next() {
            Some(s) => s.uid,
            None => return Err(anyhow!("no symbol found: '{symbol}'")),
        }
    };

    let nodes = store.impact(&uid, depth, 0.0)?;

    let rows: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "uid": n.uid,
                "name": n.name,
                "file_path": n.file_path,
                "start_line": n.start_line,
                "edge_type": n.edge_type,
                "confidence": n.confidence,
                "depth": n.depth,
            })
        })
        .collect();

    Ok(json!({
        "target": uid,
        "impact_nodes": rows,
        "total": rows.len(),
    }))
}

// ── 9. brain_guide ──────────────────────────────────────────────────────────

fn tool_schema_brain_guide() -> Value {
    json!({
        "name": "brain_guide",
        "description": "Returns the auto-generated codebase intelligence guide. Use at the start of a session to understand the indexed codebase, its repos, vaults, and available tools.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_brain_guide(store: &GraphStore, _args: Value) -> Result<Value, anyhow::Error> {
    let guide = generate_guide(store)?;
    Ok(json!({ "guide": guide }))
}

// ── 10. flow_trace ─────────────────────────────────────────────────────────

fn tool_schema_flow_trace() -> Value {
    json!({
        "name": "flow_trace",
        "description": "Use when you need to see the forward call chain from a symbol — what functions it calls, what those call, etc. Returns a tree of callees. Best for understanding execution flow from entry points.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Symbol UID to trace from" },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum traversal depth. Default 10.",
                    "default": 10
                }
            },
            "required": ["uid"]
        }
    })
}

fn tool_flow_trace(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let uid = args
        .get("uid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'uid' must be a string"))?;
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);

    // Verify root symbol exists.
    let root = store
        .lookup_symbol(uid)
        .map_err(|_| anyhow!("symbol '{uid}' not found"))?;

    let mut visited = HashSet::new();
    visited.insert(root.uid.clone());

    let tree = build_flow_tree(
        store,
        &root.uid,
        &root.name,
        &root.file_path,
        0,
        max_depth,
        &mut visited,
    );

    Ok(json!({
        "root_uid": root.uid,
        "root_name": root.name,
        "max_depth": max_depth,
        "tree": tree,
    }))
}

fn build_flow_tree(
    store: &GraphStore,
    uid: &str,
    name: &str,
    file_path: &str,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<String>,
) -> Value {
    let mut children = Vec::new();

    if depth < max_depth
        && let Ok(callees) = store.callees_of(uid)
    {
        for callee in &callees {
            if visited.contains(&callee.uid) {
                continue;
            }
            visited.insert(callee.uid.clone());
            let child = build_flow_tree(
                store,
                &callee.uid,
                &callee.name,
                &callee.file_path,
                depth + 1,
                max_depth,
                visited,
            );
            children.push(child);
        }
    }

    json!({
        "uid": uid,
        "name": name,
        "file_path": file_path,
        "depth": depth,
        "children": children,
    })
}

// ── 11. detect_changes ─────────────────────────────────────────────────────

fn tool_schema_detect_changes() -> Value {
    json!({
        "name": "detect_changes",
        "description": "Use BEFORE committing changes to understand their blast radius. Maps changed files to affected execution flows and estimates risk level.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of changed file paths (repo-relative)."
                }
            },
            "required": ["files"]
        }
    })
}

fn tool_detect_changes(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let files: Vec<String> = args
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'files' must be an array of strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if files.is_empty() {
        return Err(anyhow!("'files' must contain at least one path"));
    }

    let impact = detect_changes_impact(store, &files, 10).context("detect_changes_impact")?;

    let affected_symbols: Vec<Value> = impact
        .affected_symbols
        .iter()
        .map(|s| {
            json!({
                "uid": s.uid,
                "name": s.name,
                "file_path": s.file_path,
            })
        })
        .collect();

    let affected_processes: Vec<Value> = impact
        .affected_processes
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "uid": p.uid,
                "affected_symbol_count": p.affected_symbol_count,
                "total_symbol_count": p.total_symbol_count,
            })
        })
        .collect();

    let risk_str = match impact.risk {
        nestweaver_engine::RiskLevel::Low => "low",
        nestweaver_engine::RiskLevel::Medium => "medium",
        nestweaver_engine::RiskLevel::High => "high",
    };

    Ok(json!({
        "files": files,
        "risk": risk_str,
        "blast_radius": impact.blast_radius,
        "affected_symbols": affected_symbols,
        "affected_symbol_count": impact.affected_symbols.len(),
        "affected_processes": affected_processes,
        "affected_process_count": impact.affected_processes.len(),
    }))
}

// ── 12. clusters ───────────────────────────────────────────────────────────

fn tool_schema_clusters() -> Value {
    json!({
        "name": "clusters",
        "description": "Use to understand the high-level architecture — shows functional communities of code detected by the Leiden algorithm. Each cluster groups tightly-connected symbols.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Leiden resolution parameter (higher = more clusters). Default 1.0.",
                    "default": 1.0
                }
            }
        }
    })
}

fn tool_clusters(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let resolution = args
        .get("resolution")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);

    let output = compute_clusters(store, resolution).context("compute_clusters")?;

    let clusters_json: Vec<Value> = output
        .communities
        .iter()
        .map(|c| {
            let members: Vec<Value> = c
                .members
                .iter()
                .take(20)
                .map(|m| {
                    json!({
                        "uid": m.uid,
                        "name": m.name,
                        "file_path": m.file_path,
                    })
                })
                .collect();
            json!({
                "id": c.id,
                "name": c.name,
                "size": c.member_count,
                "cohesion": c.cohesion,
                "key_files": c.key_files,
                "members": members,
            })
        })
        .collect();

    let symbol_count: usize = output.communities.iter().map(|c| c.member_count).sum();

    Ok(json!({
        "resolution": resolution,
        "cluster_count": output.communities.len(),
        "symbol_count": symbol_count,
        "modularity": output.modularity,
        "clusters": clusters_json,
    }))
}

// ── 13. stale_check ────────────────────────────────────────────────────────

fn tool_schema_stale_check() -> Value {
    json!({
        "name": "stale_check",
        "description": "Use at the start of a session or after making changes to check if the code graph is up-to-date with the latest source. Compares indexed SHA with current git HEAD for each repo.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_stale_check(store: &GraphStore) -> Result<Value, anyhow::Error> {
    let repos = store.list_repos(None).unwrap_or_default();

    let mut results = Vec::new();
    let mut any_stale = false;

    for repo in &repos {
        // Try to determine current HEAD from the repo's URL (file:// paths).
        let current_head = if let Some(path) = repo.url.strip_prefix("file://") {
            get_git_head(path)
        } else {
            None
        };

        let is_stale = match &current_head {
            Some(head) => head != &repo.indexed_sha,
            None => repo.staleness_commits_behind > 0,
        };

        if is_stale {
            any_stale = true;
        }

        results.push(json!({
            "url": repo.url,
            "indexed_sha": repo.indexed_sha,
            "current_head": current_head,
            "is_stale": is_stale,
            "staleness_commits_behind": repo.staleness_commits_behind,
        }));
    }

    Ok(json!({
        "repo_count": repos.len(),
        "any_stale": any_stale,
        "repos": results,
    }))
}

/// Get the current HEAD sha for a git repo at the given path.
fn get_git_head(repo_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Expand a leading `~/` to the user's home directory. Returns the input
/// unchanged when no expansion is possible.
fn expand_tilde(input: &str) -> String {
    if let Some(stripped) = input.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{stripped}");
    }
    input.to_string()
}

/// Shallow check: does the directory contain any `.md` file in its tree?
/// Bounded depth to avoid blowing time on huge monorepos.
fn walk_has_markdown(root: &Path) -> bool {
    fn recurse(p: &Path, depth: u32) -> bool {
        if depth > 4 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && (ext == "md" || ext == "markdown")
                {
                    return true;
                }
            } else if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                if recurse(&path, depth + 1) {
                    return true;
                }
            }
        }
        false
    }
    recurse(root, 0)
}

// The store doesn't expose its file path. We carry it on the server side
// in a thread-local so add_source can re-open. Set by `lib.rs` before
// dispatching tool calls.
thread_local! {
    static CURRENT_DB_PATH: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static ALLOW_ADD_SOURCES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_current_db_path(path: std::path::PathBuf) {
    CURRENT_DB_PATH.with(|c| *c.borrow_mut() = Some(path));
}

pub fn set_allow_add_sources(allowed: bool) {
    ALLOW_ADD_SOURCES.with(|c| c.set(allowed));
}

fn current_db_path(_store: &GraphStore) -> Result<std::path::PathBuf, anyhow::Error> {
    CURRENT_DB_PATH.with(|c| {
        c.borrow()
            .clone()
            .ok_or_else(|| anyhow!("database path not set on server"))
    })
}

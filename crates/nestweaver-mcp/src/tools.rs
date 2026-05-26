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
    BrainContextResult, HybridSearchConfig, build_brain_context_hybrid_with_aliases,
    compute_clusters, detect_changes_impact, generate_guide, get_all_properties, index_directory,
    index_markdown_directory, load_alias_sidecar, load_extensions, query_by_property,
    save_extensions, set_property,
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
            tool_schema_set_extension(),
            tool_schema_query_extensions(),
            tool_schema_brain_diff(),
            tool_schema_project_context(),
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
        "set_extension" => tool_set_extension(args),
        "query_extensions" => tool_query_extensions(args),
        "brain_diff" => tool_brain_diff(store, args),
        "project_context" => tool_project_context(store, tantivy, args),
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
                },
                "repos": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to specific repo UIDs or names (post-PPR). Only nodes whose location matches one of these strings are kept."
                },
                "vaults": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to specific vault UIDs or names (post-PPR). Only note/heading/section nodes whose UID or location matches are kept."
                },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Include only nodes with these kind prefixes (e.g. Symbol, Note, Section, Tag, Heading). Case-insensitive prefix match against the node's kind field."
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Include only nodes whose location (file path) starts with this prefix."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Include only nodes tagged with any of these tags (applies to Note and Section nodes; Symbol nodes are always kept)."
                },
                "exclude_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exclude nodes tagged with any of these tags (applies to Note and Section nodes)."
                },
                "weight_ppr": {
                    "type": "number",
                    "description": "PPR ranking weight for hybrid RRF fusion. Default 0.7."
                },
                "weight_bm25": {
                    "type": "number",
                    "description": "BM25 text search weight for hybrid RRF fusion. Default 0.3."
                },
                "weight_semantic": {
                    "type": "number",
                    "description": "Semantic embedding weight for hybrid RRF fusion. Default 0.0 (disabled until embeddings are generated)."
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

    // RFC #2: optional post-PPR filter parameters.
    let filter_kinds: Option<Vec<String>> =
        args.get("kinds").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        });
    let filter_repos: Option<Vec<String>> =
        args.get("repos").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let filter_vaults: Option<Vec<String>> =
        args.get("vaults").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let path_prefix: Option<String> = args
        .get("path_prefix")
        .and_then(|v| v.as_str())
        .map(String::from);

    // RFC #6: optional hybrid search weight overrides.
    let defaults = HybridSearchConfig::default();
    let weight_ppr = args
        .get("weight_ppr")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_ppr)
        .max(0.0);
    let weight_bm25 = args
        .get("weight_bm25")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_bm25)
        .max(0.0);
    let weight_semantic = args
        .get("weight_semantic")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_semantic)
        .max(0.0);
    // If all weights are zero fall back to the defaults so PPR still fires.
    let (weight_ppr, weight_bm25, weight_semantic) =
        if weight_ppr == 0.0 && weight_bm25 == 0.0 && weight_semantic == 0.0 {
            (
                defaults.weight_ppr,
                defaults.weight_bm25,
                defaults.weight_semantic,
            )
        } else {
            (weight_ppr, weight_bm25, weight_semantic)
        };
    let config = HybridSearchConfig {
        weight_ppr,
        weight_bm25,
        weight_semantic,
        ..defaults
    };

    // Load taxonomy aliases so vault-defined name variants resolve correctly.
    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);

    // Hybrid retrieval whenever the Tantivy index is open. When absent
    // (cold start, index missing), falls through to pure-PPR — still
    // correct, just less recall on text-only relevance.
    let mut result: BrainContextResult =
        build_brain_context_hybrid_with_aliases(store, &seeds, tantivy, &config, &aliases)?;

    // RFC #2: apply post-PPR filters to seeds and connected lists.
    let apply_filters = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
        if let Some(ref kinds) = filter_kinds {
            nodes.retain(|n| {
                let kind_lower = n.kind.to_lowercase();
                kinds.iter().any(|k| kind_lower.starts_with(k.as_str()))
            });
        }
        if let Some(ref repos) = filter_repos {
            nodes.retain(|n| {
                repos
                    .iter()
                    .any(|r| n.uid.contains(r.as_str()) || n.location.contains(r.as_str()))
            });
        }
        if let Some(ref vaults) = filter_vaults {
            nodes.retain(|n| {
                vaults
                    .iter()
                    .any(|v| n.uid.contains(v.as_str()) || n.location.contains(v.as_str()))
            });
        }
        if let Some(ref prefix) = path_prefix {
            nodes.retain(|n| n.location.starts_with(prefix.as_str()));
        }
    };
    apply_filters(&mut result.seeds);
    apply_filters(&mut result.connected);

    // tags filter: keep only note/section nodes tagged with any of these tags.
    // Symbol nodes are always kept (no tag concept for code).
    if let Some(tags) = args.get("tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let tagged_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let tagged_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_tagged = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    if item.kind.to_lowercase().contains("symbol") {
                        return true;
                    }
                    tagged_notes.contains(&item.uid) || tagged_sections.contains(&item.uid)
                });
            };
            filter_tagged(&mut result.seeds);
            filter_tagged(&mut result.connected);
        }
    }

    // exclude_tags filter: remove note/section nodes tagged with any of these tags.
    if let Some(exclude_tags) = args.get("exclude_tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = exclude_tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let excluded_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let excluded_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_excluded = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    !excluded_notes.contains(&item.uid) && !excluded_sections.contains(&item.uid)
                });
            };
            filter_excluded(&mut result.seeds);
            filter_excluded(&mut result.connected);
        }
    }

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

    // Substring fallback: search note titles, heading text, and section bodies.
    let needle = query.to_lowercase();

    // Note title matches.
    let notes = store.list_notes(None).context("list_notes")?;
    let mut results: Vec<Value> = notes
        .iter()
        .filter(|n| n.title.to_lowercase().contains(&needle))
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

    // Heading text matches (if budget remains).
    if results.len() < limit {
        let headings = store.list_all_headings().context("list_all_headings")?;
        let remaining = limit - results.len();
        let heading_hits: Vec<Value> = headings
            .iter()
            .filter(|h| h.text.to_lowercase().contains(&needle))
            .take(remaining)
            .map(|h| {
                json!({
                    "uid": h.uid,
                    "kind": "heading",
                    "title": h.text.clone(),
                    "note_uid": h.note_uid,
                    "level": h.level,
                })
            })
            .collect();
        results.extend(heading_hits);
    }

    // Section body matches (if budget remains).
    if results.len() < limit {
        let sections = store.list_all_sections().context("list_all_sections")?;
        let remaining = limit - results.len();
        let section_hits: Vec<Value> = sections
            .iter()
            .filter(|s| s.text_content.to_lowercase().contains(&needle))
            .take(remaining)
            .map(|s| {
                json!({
                    "uid": s.uid,
                    "kind": "section",
                    "note_uid": s.note_uid,
                    "heading_uid": s.heading_uid,
                    "word_count": s.word_count,
                })
            })
            .collect();
        results.extend(section_hits);
    }

    let total = results.len();
    Ok(json!({
        "query": query,
        "engine": "substring",
        "results": results,
        "total_matches": total,
    }))
}

// ── 3. note_get ─────────────────────────────────────────────────────────────

fn tool_schema_note_get() -> Value {
    json!({
        "name": "note_get",
        "description": "Use after brain_context indicates a specific note is highly relevant and you want its full body. Loads the note's markdown from disk via vault.root_path + note.file_path, plus structural metadata (frontmatter, outline, tags, outgoing wikilink count). Pass either `uid` or `title` (title is case-insensitive and returns the first match). Use `sections` to retrieve only specific named sections instead of the full body.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (note:vlt:...:hash)" },
                "title": { "type": "string", "description": "Note title (case-insensitive)" },
                "include_body": {
                    "type": "boolean",
                    "description": "Include the full markdown body. Default true.",
                    "default": true
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of heading names. If provided, returns only those sections instead of the full body."
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

    // Parse optional section filter.
    let section_filter: Option<Vec<String>> =
        args.get("sections").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

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

    // Load all headings and sections (needed for both outline and section filter).
    let headings_raw = store.headings_in_note(&note.uid).unwrap_or_default();
    let sections_raw = store.sections_in_note(&note.uid).unwrap_or_default();

    // Resolve body: either filtered sections or full file contents.
    let body = if let Some(ref names) = section_filter {
        // Section-filter mode: return only the text_content of sections whose
        // heading matches one of the requested names (case-insensitive).
        let mut parts: Vec<String> = Vec::new();
        for heading in &headings_raw {
            if names.iter().any(|n| heading.text.eq_ignore_ascii_case(n)) {
                // Find the section that belongs to this heading.
                if let Some(sec) = sections_raw
                    .iter()
                    .find(|s| s.heading_uid.as_deref() == Some(&heading.uid))
                {
                    // Reconstruct the section with its heading prefix.
                    let prefix = "#".repeat(heading.level as usize);
                    parts.push(format!("{prefix} {}\n\n{}", heading.text, sec.text_content));
                }
            }
        }
        if parts.is_empty() {
            Some(String::new())
        } else {
            Some(parts.join("\n\n"))
        }
    } else if include_body {
        // Full body mode: load from disk.
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

    let headings = headings_raw
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

    let section_count = sections_raw.len();

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
        "section_count": section_count,
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
        .map(|v| {
            let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
            let note_count = notes.len();
            let last_indexed = notes
                .iter()
                .filter_map(|n| n.modified_at.as_deref())
                .max()
                .map(|s| s.to_string());
            json!({
                "name": v.name,
                "root_path": v.root_path,
                "note_count": note_count,
                "last_indexed": last_indexed,
            })
        })
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
    // The MCP server does not hold an InstanceConfig at runtime; cross-repo
    // edges from the graph are still included via the store query.
    let guide = generate_guide(store, None)?;
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
                "symbol": { "type": "string", "description": "Symbol name or UID to trace from" },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum traversal depth. Default 10.",
                    "default": 10
                }
            },
            "required": ["symbol"]
        }
    })
}

fn tool_flow_trace(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'symbol' must be a string"))?;
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);

    // Resolve symbol — try UID first (contains ':'), then name lookup.
    let resolved_uid = if symbol.contains(':') {
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

    // Verify root symbol exists.
    let root = store
        .lookup_symbol(&resolved_uid)
        .map_err(|_| anyhow!("symbol '{symbol}' not found"))?;

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
        // Try to determine current HEAD from the repo's URL.
        let current_head = if let Some(path) = repo.url.strip_prefix("file://") {
            get_git_head(path)
        } else {
            get_remote_head(&repo.url)
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

/// Get the current HEAD sha for a remote git repo via `git ls-remote`.
/// Works for SSH (`git@github.com:...`) and HTTPS (`https://...`) URLs.
///
/// Stderr is suppressed so SSH key errors or other diagnostics don't leak
/// into MCP responses.
fn get_remote_head(url: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--exit-code", url, "HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: "<sha>\tHEAD\n"
    stdout.split_whitespace().next().map(|s| s.to_string())
}

// ── 14. set_extension ──────────────────────────────────────────────────────

fn tool_schema_set_extension() -> Value {
    json!({
        "name": "set_extension",
        "description": "Attach a custom metadata property to a node (symbol, note, etc.) in the extension sidecar. Use to store information that isn't in the core schema — e.g. team_owner, deprecated, review_needed. Properties are stored in a JSON sidecar alongside the database and queryable with query_extensions.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": {
                    "type": "string",
                    "description": "Node UID to annotate (e.g. sym:repo:...:hash:42)"
                },
                "key": {
                    "type": "string",
                    "description": "Property name (e.g. team_owner, deprecated)"
                },
                "value": {
                    "description": "Property value — any JSON value (string, number, boolean, object, array)"
                }
            },
            "required": ["uid", "key", "value"]
        }
    })
}

fn tool_set_extension(args: Value) -> Result<Value, anyhow::Error> {
    let uid = args
        .get("uid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'uid' must be a string"))?;
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'key' must be a string"))?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| anyhow!("'value' is required"))?;

    let db_path = CURRENT_DB_PATH
        .with(|c| c.borrow().clone())
        .ok_or_else(|| anyhow!("database path not set on server"))?;

    let mut store = load_extensions(&db_path);
    set_property(&mut store, uid, key, value.clone());
    save_extensions(&db_path, &store)?;

    Ok(json!({
        "uid": uid,
        "key": key,
        "value": value,
        "status": "saved",
    }))
}

// ── 15. query_extensions ───────────────────────────────────────────────────

fn tool_schema_query_extensions() -> Value {
    json!({
        "name": "query_extensions",
        "description": "Query the extension sidecar to find nodes with a specific property value. Use to list all deprecated symbols, find nodes owned by a team, etc. Returns the UIDs and their full property maps.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Property name to filter by (e.g. team_owner)"
                },
                "value": {
                    "description": "Value to match (any JSON value)"
                },
                "uid": {
                    "type": "string",
                    "description": "Optional: return all properties for a specific node UID instead of filtering"
                }
            }
        }
    })
}

fn tool_query_extensions(args: Value) -> Result<Value, anyhow::Error> {
    let db_path = CURRENT_DB_PATH
        .with(|c| c.borrow().clone())
        .ok_or_else(|| anyhow!("database path not set on server"))?;

    let store = load_extensions(&db_path);

    // Single-UID lookup mode.
    if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        let props = get_all_properties(&store, uid);
        return Ok(json!({
            "uid": uid,
            "properties": props,
        }));
    }

    // Filter-by-key-value mode.
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("provide either 'uid' or both 'key' and 'value'"))?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| anyhow!("'value' is required when 'key' is given"))?;

    let matching_uids = query_by_property(&store, key, &value);

    let results: Vec<Value> = matching_uids
        .iter()
        .map(|uid| {
            let props = store.get(*uid).cloned().unwrap_or_default();
            json!({
                "uid": uid,
                "properties": props,
            })
        })
        .collect();

    Ok(json!({
        "key": key,
        "value": value,
        "count": results.len(),
        "results": results,
    }))
}

// ── 16. brain_diff ─────────────────────────────────────────────────────────

fn tool_schema_brain_diff() -> Value {
    json!({
        "name": "brain_diff",
        "description": "Show what changed in the graph since a given git SHA. Returns the files added/modified/deleted between the indexed SHA and the current HEAD, together with the symbols defined in those files. Use before a code review or after pulling to understand the scope of recent changes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repo name or substring of its URL to diff"
                },
                "since_sha": {
                    "type": "string",
                    "description": "Git SHA to compare against. Defaults to the repo's indexed_sha."
                }
            },
            "required": ["repo"]
        }
    })
}

fn tool_brain_diff(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    use nestweaver_engine::git_diff;

    let repo_name = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'repo' must be a string"))?;
    let since_sha_arg = args.get("since_sha").and_then(|v| v.as_str());

    // Find the repo in the graph.
    let repos = store.list_repos(None)?;
    let repo = repos
        .iter()
        .find(|r| {
            r.url.contains(repo_name) || {
                let name_part = r.url.split('/').next_back().unwrap_or("");
                name_part == repo_name
            }
        })
        .ok_or_else(|| anyhow!("repo '{}' not found in graph", repo_name))?;

    if !repo.url.starts_with("file://") {
        anyhow::bail!(
            "brain_diff only works with locally-indexed repositories (file:// URLs); \
             '{}' is not a local repo",
            repo.url
        );
    }
    let repo_path = repo.url.strip_prefix("file://").unwrap_or(&repo.url);

    let base_sha = since_sha_arg.unwrap_or(&repo.indexed_sha);

    // Get the current HEAD SHA.
    let head_sha = git_diff::current_head_sha(std::path::Path::new(repo_path))
        .unwrap_or_else(|_| "unknown".to_string());

    // If base == head there is nothing to show.
    if base_sha == head_sha {
        return Ok(json!({
            "repo": repo_name,
            "base_sha": base_sha,
            "head_sha": head_sha,
            "files_added": 0,
            "files_modified": 0,
            "files_deleted": 0,
            "changed_files": [],
            "affected_symbols": [],
            "message": "graph is up to date with HEAD",
        }));
    }

    let changes = git_diff::detect_changes(std::path::Path::new(repo_path), base_sha, &head_sha)
        .context("git diff")?;

    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();

    for change in &changes {
        match change {
            git_diff::FileChange::Added(p) => added.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Modified(p) => modified.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Deleted(p) => deleted.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Renamed { to, .. } => {
                modified.push(to.to_string_lossy().into_owned())
            }
        }
    }

    // Collect symbols from the changed/added files.
    let changed_paths: Vec<&str> = added
        .iter()
        .chain(modified.iter())
        .map(String::as_str)
        .collect();

    let mut affected_symbols: Vec<Value> = Vec::new();
    for file_path in &changed_paths {
        if let Ok(syms) = store.symbols_in_file(file_path) {
            for sym in syms {
                affected_symbols.push(json!({
                    "uid": sym.uid,
                    "name": sym.name,
                    "kind": sym.kind,
                    "file_path": sym.file_path,
                    "start_line": sym.start_line,
                }));
            }
        }
    }

    let all_changed: Vec<&str> = added
        .iter()
        .chain(modified.iter())
        .map(String::as_str)
        .collect();

    Ok(json!({
        "repo": repo_name,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "files_added": added.len(),
        "files_modified": modified.len(),
        "files_deleted": deleted.len(),
        "added_files": added,
        "modified_files": modified,
        "deleted_files": deleted,
        "changed_files": all_changed,
        "affected_symbols": affected_symbols,
        "affected_symbol_count": affected_symbols.len(),
    }))
}

// ── 17. project_context ────────────────────────────────────────────────────

fn tool_schema_project_context() -> Value {
    json!({
        "name": "project_context",
        "description": "Return all Notes, Symbols, and Sections associated with a Project, ranked by PPR within the project's subgraph. Use when you need to understand or work on a specific project.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project name, alias, or UID"
                },
                "token_budget": {
                    "type": "integer",
                    "default": 3000,
                    "description": "Approximate token cap for the result (chars / 4)"
                },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter result kinds (e.g. Symbol, Note, Section)"
                },
                "include_components": {
                    "type": "boolean",
                    "default": true,
                    "description": "For composite projects, also include notes/symbols from component sub-projects"
                }
            },
            "required": ["project"]
        }
    })
}

fn tool_project_context(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
) -> Result<Value, anyhow::Error> {
    let project_str = args
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'project' must be a string"))?;
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(3000);
    let include_components = args
        .get("include_components")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let filter_kinds: Option<Vec<String>> =
        args.get("kinds").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        });

    // 1. Resolve the project: name/alias/UID.
    let project = if let Some(uid) = project_str.strip_prefix("proj:") {
        // Direct UID — list all projects and find by uid.
        let all = store
            .list_projects()
            .map_err(|e| anyhow!("list_projects: {e}"))?;
        all.into_iter()
            .find(|p| p.uid == uid || p.uid == project_str)
            .ok_or_else(|| anyhow!("project UID '{}' not found", project_str))?
    } else {
        // Try name match first.
        match store
            .lookup_project_by_name(project_str)
            .map_err(|e| anyhow!("lookup_project_by_name: {e}"))?
        {
            Some(p) => p,
            None => {
                // Try aliases: load all projects, check aliases field via extension sidecar.
                // For now fall back to checking if the string is a UID substring.
                let all = store
                    .list_projects()
                    .map_err(|e| anyhow!("list_projects: {e}"))?;
                all.into_iter()
                    .find(|p| p.uid.contains(project_str))
                    .ok_or_else(|| anyhow!("project '{}' not found", project_str))?
            }
        }
    };

    // 2. Collect target UIDs: notes + symbols for this project.
    let mut seed_uids: Vec<String> = Vec::new();
    let note_uids = store
        .list_project_note_uids(&project.uid)
        .map_err(|e| anyhow!("list_project_note_uids: {e}"))?;
    seed_uids.extend(note_uids);
    let sym_uids = store
        .list_project_symbol_uids(&project.uid)
        .map_err(|e| anyhow!("list_project_symbol_uids: {e}"))?;
    seed_uids.extend(sym_uids);

    // 3. If include_components, also collect note/symbol UIDs from each component project.
    if include_components {
        let component_uids = store
            .list_project_component_uids(&project.uid)
            .map_err(|e| anyhow!("list_project_component_uids: {e}"))?;
        for comp_uid in &component_uids {
            let comp_notes = store.list_project_note_uids(comp_uid).unwrap_or_default();
            seed_uids.extend(comp_notes);
            let comp_syms = store.list_project_symbol_uids(comp_uid).unwrap_or_default();
            seed_uids.extend(comp_syms);
        }
    }

    // Deduplicate seeds.
    let mut seen = std::collections::HashSet::new();
    seed_uids.retain(|u| seen.insert(u.clone()));

    if seed_uids.is_empty() {
        return Ok(json!({
            "project": project.name,
            "project_uid": project.uid,
            "seeds": [],
            "connected": [],
            "unresolved_seeds": [],
            "tokens_used": 0,
            "token_budget": token_budget,
            "truncated": false,
            "total_connected": 0,
            "note": "No notes or symbols are associated with this project yet.",
        }));
    }

    // 4. Run hybrid PPR from seeds.
    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);
    let config = HybridSearchConfig::default();
    let mut result =
        build_brain_context_hybrid_with_aliases(store, &seed_uids, tantivy, &config, &aliases)?;

    // 5. Apply optional kinds filter.
    if let Some(ref kinds) = filter_kinds {
        let apply_kinds = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
            nodes.retain(|n| {
                let kind_lower = n.kind.to_lowercase();
                kinds.iter().any(|k| kind_lower.starts_with(k.as_str()))
            });
        };
        apply_kinds(&mut result.seeds);
        apply_kinds(&mut result.connected);
    }

    // 6. Apply token budget.
    let (cut, used_tokens) = budgeted_cut(&result.connected, token_budget);

    // 7. Load external_refs from extension sidecar.
    let ext_store = load_extensions(&db_path);
    let external_refs = get_all_properties(&ext_store, &project.uid)
        .get("external_refs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

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
        "project": project.name,
        "project_uid": project.uid,
        "seeds": seeds_json,
        "connected": connected_json,
        "unresolved_seeds": result.unresolved_seeds,
        "tokens_used": used_tokens,
        "token_budget": token_budget,
        "truncated": cut < result.connected.len(),
        "total_connected": result.connected.len(),
        "external_refs": external_refs,
    }))
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
    static ALLOW_ADD_SOURCES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
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

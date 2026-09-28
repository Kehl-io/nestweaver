//! The `memory` subcommand family and its text renderers.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

pub(crate) fn run_memory(
    command: MemoryCommands,
    t0: std::time::Instant,
    use_daemon: bool,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        MemoryCommands::Lint {
            json,
            limit,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_existing_db(&db_path)?;
            let mut args = serde_json::json!({});
            if let Some(limit) = limit {
                args["limit"] = serde_json::json!(limit);
            }
            let payload = dispatch_mcp_json(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_memory_lint",
                args,
            )?;
            if json {
                print_json_payload(&payload)?;
            } else {
                print_memory_lint_text(&payload);
            }
            let issues = memory_lint_issue_count(&payload);
            let stats = format!("{} issue(s) in {}", issues, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        MemoryCommands::Consolidate {
            apply,
            json,
            limit,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_existing_db(&db_path)?;
            let mut args = serde_json::json!({ "apply": apply });
            if let Some(limit) = limit {
                args["limit"] = serde_json::json!(limit);
            }
            let payload = if apply {
                match try_hybrid_json_rpc_checked(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_memory_consolidate",
                    args.clone(),
                ) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        let write_lease =
                            require_exclusive_store_access(&db_path, "apply memory consolidation")?;
                        let store = GraphStore::open_with_authority(&db_path, &write_lease)
                            .map_err(|error| daemon_held_store_error(&db_path, error))?;
                        nestweaver_mcp::tools::with_authoritative_writer_ownership(|| {
                            nestweaver_mcp::tools::dispatch(
                                &store,
                                None,
                                "brain_memory_consolidate",
                                args,
                                None,
                            )
                        })?
                    }
                    Err(error) => return Err(error),
                }
            } else {
                dispatch_mcp_json(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_memory_consolidate",
                    args,
                )?
            };
            if json {
                print_json_payload(&payload)?;
            } else {
                print_memory_consolidate_text(&payload);
            }
            let proposal_count = payload
                .get("proposals_total")
                .and_then(|total| total.as_u64())
                .or_else(|| {
                    payload
                        .get("proposals")
                        .and_then(|items| items.as_array())
                        .map(|items| items.len() as u64)
                })
                .unwrap_or(0);
            let failed_apply = apply
                && !payload
                    .get("applied")
                    .and_then(|applied| applied.as_bool())
                    .unwrap_or(false)
                && proposal_count > 0;
            if failed_apply {
                anyhow::bail!(
                    "memory consolidation apply did not complete; review the warnings above and recover the durable journal before retrying"
                );
            }
            let stats = format!(
                "{} proposal(s) in {}",
                proposal_count,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        MemoryCommands::Related {
            uid,
            edge_types,
            depth,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_existing_db(&db_path)?;
            let mut args = serde_json::json!({
                "uid": uid,
                "edge_types": edge_types,
                "depth": depth,
            });
            if let Some(limit) = limit {
                args["limit"] = serde_json::json!(limit);
            }
            let payload = dispatch_mcp_json(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_memory_related",
                args,
            )?;
            // nw-524: mirrors `note get` -- a `not_found` uid is a named
            // refusal at exit 2, not the same `related: []` shape a present
            // note with zero typed relations gets at exit 0.
            if payload["status"].as_str() == Some("not_found") {
                if json {
                    print_json_not_found("uid", &uid);
                }
                eprintln!("Note '{uid}' not found.");
                return Ok((EXIT_NOT_FOUND, None));
            }
            if json {
                print_json_payload(&payload)?;
            } else {
                print_memory_related_text(&uid, &payload);
            }
            let neighbour_count = payload
                .get("returned")
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    payload
                        .get("related")
                        .and_then(|items| items.as_array())
                        .map(|items| items.len() as u64)
                })
                .unwrap_or(0);
            let stats = format!(
                "{} neighbour(s) in {}",
                neighbour_count,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }
    }
}

pub(crate) fn dispatch_mcp_json(
    use_daemon: bool,
    db_path: &Path,
    config: Option<&Path>,
    tool: &str,
    args: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    match try_hybrid_json_rpc_checked(use_daemon, db_path, config, tool, args.clone()) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => {
            let store = open_store(Some(db_path))?;
            nestweaver_mcp::tools::dispatch(&store, None, tool, args, None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn json_len(payload: &serde_json::Value, key: &str) -> usize {
    payload
        .get(key)
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0)
}

pub(crate) fn json_total(payload: &serde_json::Value, key: &str) -> usize {
    payload
        .get(format!("{key}_total"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(|| json_len(payload, key))
}

pub(crate) fn memory_lint_issue_count(payload: &serde_json::Value) -> usize {
    json_total(payload, "stale")
        + json_total(payload, "contradictions")
        + json_total(payload, "supersession_chains")
        + json_total(payload, "schema_drift")
        + json_total(payload, "dangling_relationships")
}

pub(crate) fn json_count_label(payload: &serde_json::Value, key: &str) -> String {
    let returned = json_len(payload, key);
    let total = json_total(payload, key);
    if total > returned {
        format!("{returned} of {total}")
    } else {
        returned.to_string()
    }
}

pub(crate) fn print_memory_lint_text(payload: &serde_json::Value) {
    println!("Memory lint:");
    println!(
        "  stale notes:           {}",
        json_count_label(payload, "stale")
    );
    println!(
        "  contradictions:        {}",
        json_count_label(payload, "contradictions")
    );
    println!(
        "  orphans:               {}",
        json_count_label(payload, "orphans")
    );
    let broken = payload
        .get("broken_wikilinks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let lint_unresolved = broken
        .iter()
        .filter(|link| link.get("resolved_target_uid").is_none_or(|v| v.is_null()))
        .count();
    let broken_total = json_total(payload, "broken_wikilinks");
    let broken_count = if broken_total > broken.len() {
        format!("{} of {broken_total}", broken.len())
    } else {
        broken.len().to_string()
    };
    println!(
        "  broken wikilinks:      {broken_count} ({} genuinely broken, {} lower-tier resolutions)",
        lint_unresolved,
        broken.len().saturating_sub(lint_unresolved)
    );
    println!(
        "  supersession chains:   {}",
        json_count_label(payload, "supersession_chains")
    );
    println!(
        "  schema drift:          {}",
        json_count_label(payload, "schema_drift")
    );
    println!(
        "  dangling relationships: {}",
        json_count_label(payload, "dangling_relationships")
    );
    if let Some(stale) = payload["stale"].as_array() {
        for s in stale {
            println!(
                "  stale: {} ({} days)",
                s["file_path"].as_str().unwrap_or("?"),
                s["days_stale"].as_u64().unwrap_or(0)
            );
        }
    }
    if let Some(contradictions) = payload["contradictions"].as_array() {
        for c in contradictions {
            let cycle = c["cycle"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                })
                .unwrap_or_default();
            println!("  contradiction cycle: {cycle}");
        }
    }
    if let Some(dangling) = payload["dangling_relationships"].as_array() {
        for d in dangling {
            println!(
                "  dangling: {} -[{}]-> {} (missing)",
                d["source_uid"].as_str().unwrap_or("?"),
                d["edge_type"].as_str().unwrap_or("?"),
                d["target_uid"].as_str().unwrap_or("?")
            );
        }
    }
}

pub(crate) fn print_memory_consolidate_text(payload: &serde_json::Value) {
    let dry_run = payload
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    println!(
        "Consolidation ({}):",
        if dry_run { "dry-run" } else { "apply" }
    );
    if let Some(warnings) = payload["warnings"].as_array() {
        for w in warnings {
            if let Some(text) = w.as_str() {
                println!("  warning: {text}");
            }
        }
    }
    let proposals = payload
        .get("proposals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let proposals_total = json_total(payload, "proposals");
    if proposals_total > proposals.len() {
        println!("  {} of {proposals_total} proposal(s):", proposals.len());
    }
    if proposals.is_empty() {
        println!("  no promotion candidates.");
    } else {
        for p in &proposals {
            println!(
                "  promote {} → {}",
                p["source_path"].as_str().unwrap_or("?"),
                p["promote_to"].as_str().unwrap_or("?")
            );
            if let Some(rationale) = p["rationale"].as_str() {
                println!("    {rationale}");
            }
        }
    }
}

pub(crate) fn print_memory_related_text(uid: &str, payload: &serde_json::Value) {
    let related = payload
        .get("related")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if related.is_empty() {
        println!("No typed neighbours found for {uid}.");
        return;
    }
    let related_total = payload
        .get("total")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(related.len());
    if related_total > related.len() {
        println!(
            "Typed neighbours of {uid} ({} of {related_total}):",
            related.len()
        );
    } else {
        println!("Typed neighbours of {uid} ({}):", related.len());
    }
    for r in &related {
        println!(
            "  [{}] {} — {} (via {})",
            r["depth"].as_u64().unwrap_or(0),
            r["title"].as_str().unwrap_or("?"),
            r["file_path"].as_str().unwrap_or("?"),
            r["via_edge"].as_str().unwrap_or("?")
        );
    }
}

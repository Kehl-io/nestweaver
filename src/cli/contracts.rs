//! The `contracts` command family.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

/// Resolve a `--repo` filter (display name or literal UID) to a repo UID.
/// Returns `Ok(None)` when no filter was given, or an error when the filter
/// matches no indexed repo.
pub(crate) fn resolve_contract_repo_filter(
    store: &GraphStore,
    filter: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    // The shared typed resolver: its failure class decides the exit code.
    let resolved = resolve_repo_filter(store, &[filter.to_string()])?;
    Ok(resolved.into_iter().next())
}

pub(crate) fn render_contract_list(
    contracts: &[nestweaver_schema::Contract],
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(contracts)?);
    } else if contracts.is_empty() {
        println!(
            "No contracts found. Index a repo with OpenAPI/proto/GraphQL specs or \
             Spring/NestJS controllers first."
        );
    } else {
        println!(
            "API contracts ({} total). NOTE: contract links are hypotheses, \
             not ground truth — see confidence.\n",
            contracts.len()
        );
        for contract in contracts {
            println!("{}", contract.uid);
            println!("  kind:       {}", contract.kind);
            if let Some(ref verb) = contract.verb {
                println!("  verb:       {verb}");
            }
            if let Some(ref path) = contract.path {
                println!("  path:       {path}");
            }
            if let Some(ref operation) = contract.operation_id {
                println!("  operation:  {operation}");
            }
            println!("  source:     {}", contract.source_path);
            println!("  confidence: {:.2}", contract.confidence);
            println!();
        }
    }
    Ok(())
}

pub(crate) fn list_contracts_via_daemon(
    db_path: &std::path::Path,
    repo: Option<&str>,
) -> anyhow::Result<Vec<nestweaver_schema::Contract>> {
    let runtime = tokio::runtime::Runtime::new().context("create runtime for contracts list")?;
    runtime
        .block_on(async {
            let mut client = nestweaver_client::DaemonClient::connect(db_path, None).await?;
            client.list_contracts(repo).await
        })
        .with_context(|| {
            format!(
                "daemon contracts list failed for {}; refusing direct-store fallback while the daemon owns the database",
                db_path.display()
            )
        })
}

/// Human rendering of the daemon `contract_drift` result (`contracts drift`
/// without `--json`).
pub(crate) fn render_contract_drift_human(value: &serde_json::Value) {
    let dni = value
        .get("declared_not_implemented")
        .and_then(|v| v.as_array());
    let ind = value
        .get("implemented_not_declared")
        .and_then(|v| v.as_array());
    // Trust signal first: a repo whose contract derivation failed has zero
    // findings for the wrong reason, and "No contract drift detected." on that
    // repo is a false clean bill of health.
    let degraded: Vec<&str> = value
        .get("degraded_repos")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if !degraded.is_empty() {
        println!(
            "Contract derivation FAILED for {} repo(s): {}",
            degraded.len(),
            degraded.join(", ")
        );
        println!(
            "Drift results below are STALE (last successful derivation) — re-index to refresh them.\n"
        );
    }
    let empty = |a: Option<&Vec<serde_json::Value>>| a.map(|v| v.is_empty()).unwrap_or(true);
    if empty(dni) && empty(ind) {
        if degraded.is_empty() {
            println!("No contract drift detected.");
        } else {
            println!("No contract drift reported, but the analysis is degraded (see above).");
        }
        return;
    }
    println!("Contract drift (hypotheses, not ground truth):\n");
    for (label, arr) in [
        ("Declared but NOT implemented", dni),
        ("Implemented but NOT declared in any spec", ind),
    ] {
        if let Some(arr) = arr.filter(|a| !a.is_empty()) {
            println!("{label} ({}):", arr.len());
            for f in arr {
                if let Some(uid) = f.get("uid").and_then(|v| v.as_str()) {
                    println!("  - {uid}");
                }
            }
            println!();
        }
    }
}

pub(crate) fn run_contracts(
    command: ContractCommands,
    use_daemon: bool,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        ContractCommands::List { repo, json, db } => {
            let db_path = db.clone().unwrap_or_else(default_db_path);
            // Read-only query: reject a typo'd path before daemon autostart can
            // create an empty database and false-green with an empty list.
            require_existing_db(&db_path)?;
            let listed = if use_daemon {
                list_contracts_via_daemon(&db_path, repo.as_deref())
            } else {
                let store = open_store(Some(&db_path))?;
                resolve_contract_repo_filter(&store, repo.as_deref()).and_then(|repo_uid| {
                    let mut contracts = store
                        .list_contracts(repo_uid.as_deref())
                        .map_err(|e| anyhow::anyhow!(e))?;
                    contracts.sort_by(|left, right| left.uid.cmp(&right.uid));
                    Ok(contracts)
                })
            };
            let contracts = match listed {
                Err(error) if error_is_unresolved_repo_filter(&error) => {
                    return Ok((report_unresolved_repo_filter(&error, json), None));
                }
                other => other?,
            };
            render_contract_list(&contracts, json)?;
            Ok((EXIT_SUCCESS, None))
        }
        ContractCommands::Drift {
            repo,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_openable_db(&db_path)?;
            // ── daemon guard ──────────────────────────────────────
            if use_daemon {
                let mut args = serde_json::json!({});
                if let Some(ref r) = repo {
                    args["repo"] = serde_json::json!(r);
                }
                let answer = match try_hybrid_json_rpc_checked(
                    true,
                    &db_path,
                    config.as_deref(),
                    "contract_drift",
                    args,
                ) {
                    Err(error) if error_is_unresolved_repo_filter(&error) => {
                        return Ok((report_unresolved_repo_filter(&error, json), None));
                    }
                    other => other?,
                };
                if let Some(value) = answer {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else {
                        render_contract_drift_human(&value);
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }
            let store = open_store(Some(&db_path))?;
            let repo_uid = match resolve_contract_repo_filter(&store, repo.as_deref()) {
                Err(error) if error_is_unresolved_repo_filter(&error) => {
                    return Ok((report_unresolved_repo_filter(&error, json), None));
                }
                other => other?,
            };
            let report = nestweaver_engine::contracts::drift_for_store(&store, repo_uid.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;

            // nw-097 family: this path used to print a BARE `DriftReport` while
            // the daemon path printed the MCP envelope — different keys, no
            // totals, no `clean`, and no limit truncation at all, so the JSON
            // shape depended on whether a daemon happened to be running. Build
            // the same envelope from the same builder and render it with the
            // same renderer.
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(
                None,
                cfg.as_ref(),
                nestweaver_engine::config::DEFAULT_RESULT_LIMIT,
            );
            let value = nestweaver_engine::contracts::drift_envelope(report, limit);
            if json {
                // nw-117/nw-347: `_meta` is added by the federation layer, which
                // only runs on the daemon path. This result is genuinely local
                // scope and the emitter says so, keeping ONE shape in both modes.
                print_json_payload(&value)?;
            } else {
                render_contract_drift_human(&value);
            }
            Ok((EXIT_SUCCESS, None))
        }
        ContractCommands::Diff {
            base,
            head,
            json,
            fail_on_breaking,
        } => {
            let base_src = std::fs::read_to_string(&base)
                .with_context(|| format!("read base spec {}", base.display()))?;
            let head_src = std::fs::read_to_string(&head)
                .with_context(|| format!("read head spec {}", head.display()))?;
            let changes = nestweaver_engine::contracts::diff_openapi(
                &base.to_string_lossy(),
                &base_src,
                &head.to_string_lossy(),
                &head_src,
            )
            .ok_or_else(|| {
                anyhow::anyhow!("both files must be parseable OpenAPI specs (yaml/json)")
            })?;
            let breaking = changes
                .iter()
                .filter(|c| {
                    c.severity == nestweaver_engine::contracts::SpecChangeSeverity::Breaking
                })
                .count();
            if json {
                println!("{}", serde_json::to_string_pretty(&changes)?);
            } else if changes.is_empty() {
                println!("No API changes detected.");
            } else {
                for c in &changes {
                    let sev = match c.severity {
                        nestweaver_engine::contracts::SpecChangeSeverity::Breaking => "BREAKING",
                        nestweaver_engine::contracts::SpecChangeSeverity::Info => "INFO",
                    };
                    println!("  [{sev}] {} {} — {}", c.verb, c.path, c.detail);
                }
                println!("\n{breaking} breaking, {} info", changes.len() - breaking);
            }
            let exit = if fail_on_breaking && breaking > 0 {
                EXIT_ERROR
            } else {
                EXIT_SUCCESS
            };
            Ok((exit, None))
        }
    }
}

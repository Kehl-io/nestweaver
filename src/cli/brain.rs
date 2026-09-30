//! The `brain` subcommand family dispatcher.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

pub(crate) fn require_complete_graph_publication(
    operation: &str,
    publication: &nestweaver_engine::manifest::GraphMutationPublicationOutcome,
) -> anyhow::Result<()> {
    use nestweaver_engine::manifest::GraphMutationPublicationDisposition;

    if publication.disposition != GraphMutationPublicationDisposition::CommittedDegraded {
        return Ok(());
    }
    let warnings = if publication.warnings.is_empty() {
        "unspecified publication stage".to_string()
    } else {
        publication
            .warnings
            .iter()
            .map(|warning| format!("{}: {}", warning.stage, warning.message))
            .collect::<Vec<_>>()
            .join("; ")
    };
    anyhow::bail!(
        "{operation} committed graph changes (generation {} -> {}), but publication reconciliation is degraded: {warnings}. The graph was NOT rolled back; repair the named stage(s) before treating derived artifacts as current",
        publication.generation_before,
        publication.generation_after,
    )
}

pub(crate) fn run_brain(
    command: BrainCommands,
    out: &OutputConfig,
    t0: std::time::Instant,
    use_daemon: bool,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        BrainCommands::Add {
            path,
            name,
            instance,
            db,
            config,
            ignore,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            // Create-operation (like `index`): create a missing parent
            // directory for --db up front instead of failing deep inside
            // the store open with a bare OS error.
            ensure_db_parent_dir(&db_path)?;
            // nw-019: --instance flag > config's instance_id > "default".
            let instance_id_owned =
                resolve_instance_id_for_db(instance, config.as_deref(), &db_path)?;
            let instance_id = instance_id_owned.as_str();
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });

            if !path.exists() {
                eprintln!("Error: path does not exist: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            if !path.is_dir() {
                eprintln!("Error: path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }

            // Auto-detect: report what we think the source is.
            let has_obsidian = path.join(".obsidian").is_dir();
            let kind_hint = if has_obsidian {
                "Obsidian vault"
            } else {
                "markdown folder"
            };
            out.status(&format!(
                "Detected {} at {} -> {}",
                kind_hint,
                path.display(),
                db_path.display()
            ));

            let extra_patterns = parse_ignore_flag(&ignore);
            let note_limits = note_limits_from_config(config.as_deref())?;

            if use_daemon {
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ))?;
                // Absolute path: the daemon runs with CWD=/ and would otherwise resolve
                // a client-relative vault path against the wrong directory (indexing 0).
                let vault_abs = abs_for_daemon(&path);
                let req = nestweaver_proto::IndexVaultRequest {
                    vault_path: vault_abs.to_string_lossy().to_string(),
                    vault_name: vault_name.clone(),
                    extra_ignore_patterns: extra_patterns.clone(),
                    instance_id: instance_id.to_string(),
                    max_note_bytes: rpc_max_note_bytes(config.as_deref(), note_limits),
                };
                // nw-192: the daemon formats only a COUNT into its terminal
                // message ("...; N eligible file(s) skipped"), while the
                // direct path below prints "  {path} - {reason}" per file.
                // IndexProgress has carried the per-file `skipped_files` all
                // along and `index` already prints them; only this vault path
                // dropped them on the floor. A reporter saw 210 of 1,244 .md
                // files skipped with no way to learn why, because the daemon
                // serves this by default.
                let mut skipped_files: Vec<nestweaver_proto::IndexSkipDetail> = Vec::new();
                rt.block_on(async {
                    let stream = client.inner_mut().index_vault(req).await?.into_inner();
                    consume_cli_index_progress(stream, |progress| {
                        let phase_name = match progress.phase {
                            0 => "Discovering",
                            1 => "Parsing",
                            2 => "Resolving",
                            3 => "Writing",
                            4 => "PageRank",
                            5 => "Done",
                            6 => "Error",
                            _ => "Unknown",
                        };
                        eprintln!("[{phase_name}] {}", progress.message);
                        // REPLACE, don't extend: the daemon emits the same
                        // full list on more than one phase message, so
                        // accumulating double-counted every skip.
                        if !progress.skipped_files.is_empty() {
                            skipped_files = progress.skipped_files.clone();
                        }
                    })
                    .await
                })?;
                if !skipped_files.is_empty() {
                    out.status(&format!("Skipped {} file(s):", skipped_files.len()));
                    for file in &skipped_files {
                        out.status(&format!(
                            "  {} — {}: {}",
                            file.path, file.reason_code, file.detail
                        ));
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            // Direct-write fallback (`--no-daemon`). It writes the graph and
            // the Tantivy index; neither checked for a live daemon holding the
            // same database.
            // nw-684 review: acquiring the write lease creates the database
            // file, so refuse over an unloadable `.brainignore` first -- a
            // refused first add must not leave an empty database behind.
            nestweaver_engine::load_brain_ignore(&path, &extra_patterns)?;
            let write_lease = require_exclusive_store_access(&db_path, "add a vault")?;
            let result = index_markdown_directory_with_ignore_and_write_lease_and_note_limits(
                &path,
                &db_path,
                instance_id,
                &vault_name,
                &extra_patterns,
                note_limits,
                &write_lease,
            )
            .context("index_markdown_directory")?;

            // Record the indexer run timestamp for this vault.
            if let Err(e) = record_last_indexed_at(&db_path, &result.vault_uid) {
                tracing::warn!("failed to record last_indexed_at: {e}");
            }

            let notes_count = result.notes_count;

            if result.notes_count == 0 {
                // The Vault node was created, but no markdown files were
                // found. Tell the user clearly rather than print a row of
                // zeros that looks like an indexing bug.
                println!(
                    "No markdown files found in {}. Vault '{}' was registered \
                     so the watcher can pick up notes added later.",
                    path.display(),
                    result.vault_name,
                );
            } else {
                println!(
                    "Indexed vault '{}': {} note(s), {} heading(s), {} section(s), \
                     {} tag(s), {} wikilink edge(s), {} unresolved link \
                     occurrence(s) across {} distinct target(s).",
                    result.vault_name,
                    result.notes_count,
                    result.headings_count,
                    result.sections_count,
                    result.tags_count,
                    result.resolved_link_edges,
                    result.unresolved_link_occurrences,
                    result.unresolved_link_targets,
                );
            }

            // Link notes to code (nw-675: the same reconciliation the
            // daemon route relies on, so the two routes cannot drift).
            reconcile_code_links_direct(
                &db_path,
                &write_lease,
                config.as_deref(),
                &format!("index of vault {}", path.display()),
            );

            // Auto-populate Tantivy BM25 index after brain add so that
            // `brain search` works immediately without a manual reindex.
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            match TantivyIndex::open_or_create(&tantivy_path) {
                Ok(tantivy) => {
                    let store_for_tantivy =
                        GraphStore::open_read_only_with_authority(&db_path, &write_lease)?;
                    match tantivy.reindex_from_store(&store_for_tantivy) {
                        Ok(count) => out.status(&format!("Tantivy: indexed {count} document(s)")),
                        Err(e) => tracing::warn!("Tantivy reindex failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("Tantivy open failed: {e}"),
            }

            if !result.skipped.is_empty() {
                out.status(&format!("Skipped {} file(s):", result.skipped.len()));
                for sf in &result.skipped {
                    out.status(&format!("  {} - {}", sf.path, sf.reason));
                }
            }
            // nw-585: indexed, but without their frontmatter. The daemon
            // route prints the same lines inside its terminal message.
            if let Some(unparsed) = nestweaver_engine::index_md::frontmatter_unparsed_summary(
                &result.frontmatter_unparsed,
            ) {
                out.status(&unparsed);
            }

            let stats = format!("{} notes in {}", notes_count, format_elapsed(t0.elapsed()));
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::List { json, db, config } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let value = if let Some(value) = try_hybrid_json_rpc(
                use_daemon,
                &db_path,
                config.as_deref(),
                "list_vaults",
                serde_json::json!({"include_counts": true}),
            )? {
                value
            } else {
                let store = open_store(Some(&db_path))?;
                nestweaver_engine::index_md::vault_inventory(&store, &db_path)?
            };
            let rows = value
                .as_array()
                .or_else(|| value.get("results").and_then(serde_json::Value::as_array))
                .context("invalid vault inventory response")?;
            if json {
                println!("{}", serde_json::to_string_pretty(rows)?);
            } else if rows.is_empty() {
                println!("No vaults indexed. Try: nestweaver brain add <path>");
            } else {
                for row in rows {
                    println!(
                        "{}\n  UID:   {}\n  Path:  {}\n  Notes: {}\n  Last indexed: {}",
                        row["name"].as_str().unwrap_or("(unknown)"),
                        row["uid"].as_str().unwrap_or("(unknown)"),
                        row["root_path"].as_str().unwrap_or("(unknown)"),
                        row["notes"]
                            .as_u64()
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "unavailable".into()),
                        row["last_indexed"].as_str().unwrap_or("(unknown)")
                    );
                    if let Some(error) = row["inventory_error"].as_str() {
                        println!("  Warning: {error}");
                    }
                }
            }
            // nw-587: a vault this database was asked to hold that the graph
            // no longer has (a WAL move-aside drops whatever the log held) is
            // NAMED, with the command that restores it. stderr in both modes,
            // so the `--json` array keeps its shape. Same engine check
            // `brain status` forwards as a `vault_registration_missing` warning.
            let live = rows
                .iter()
                .filter_map(|row| Some((row["uid"].as_str()?, row["root_path"].as_str()?)));
            match nestweaver_engine::vault_registration::missing(&db_path, live) {
                Ok(missing) => {
                    for entry in &missing {
                        eprintln!(
                            "Warning: {}\n  Run: {}",
                            nestweaver_engine::vault_registration::missing_warning(entry),
                            nestweaver_engine::vault_registration::readd_command(&db_path, entry),
                        );
                    }
                }
                Err(error) => eprintln!(
                    "Warning: cannot tell whether any registered vault is missing from the graph: {error:#}"
                ),
            }
            let unavailable = rows.iter().any(|r| r["notes"].is_null());
            Ok((
                if unavailable {
                    EXIT_ERROR
                } else {
                    EXIT_SUCCESS
                },
                None,
            ))
        }

        BrainCommands::Status { json, db, config } => {
            let db_resolved = resolve_db_with_config(db, config.as_deref())?;
            let db_path = db_resolved.as_path();

            // ── daemon guard ──────────────────────────────────────
            if let Some(value) = try_hybrid_json_rpc_checked(
                use_daemon,
                db_path,
                config.as_deref(),
                "brain_status",
                serde_json::json!({}),
            )? {
                if json {
                    // Inject upstream info into JSON output.
                    let mut value = value;
                    let upstream_configs = nestweaver_client::discovery::discover_upstreams(
                        db_path.parent().unwrap_or(std::path::Path::new(".")),
                    );
                    if !upstream_configs.is_empty() {
                        let upstreams_json: Vec<_> = upstream_configs
                            .iter()
                            .map(|ucfg| {
                                serde_json::json!({
                                    "name": ucfg.name.as_deref().unwrap_or("upstream"),
                                    "url": ucfg.url,
                                    "mode": format!("{:?}", ucfg.mode).to_lowercase(),
                                })
                            })
                            .collect();
                        if let Some(obj) = value.as_object_mut() {
                            obj.insert("upstreams".to_string(), serde_json::json!(upstreams_json));
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    println!("Brain status:");
                    println!("  Database:  {}", db_path.display());
                    let vault_count = value
                        .get("vault_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!("  Vaults:    {}", vault_count);
                    if let Some(vaults) = value.get("vaults").and_then(|v| v.as_array()) {
                        // When two rows share a name we annotate with the
                        // instance_id so the user can target precise removes.
                        // Empty-named rows (phantom registrations) always
                        // get annotated so they don't render as blank lines.
                        let mut name_counts: std::collections::HashMap<&str, usize> =
                            std::collections::HashMap::new();
                        for v in vaults {
                            let name = v["name"].as_str().unwrap_or("?");
                            *name_counts.entry(name).or_insert(0) += 1;
                        }
                        for v in vaults {
                            let name = v["name"].as_str().unwrap_or("?");
                            // nw-269: the MCP route nulls this when a
                            // per-vault `list_notes` fails; `unwrap_or(0)`
                            // printed that as "0 notes".
                            let note_count = render_optional_count(v.get("note_count"));
                            let last_indexed = v["last_indexed"].as_str().unwrap_or("never");
                            let ambiguous = name_counts.get(name).copied().unwrap_or(0) > 1;
                            let unnamed = name.is_empty();
                            if ambiguous || unnamed {
                                let instance = v["instance_id"].as_str().unwrap_or("?");
                                let root_path = v["root_path"].as_str().unwrap_or("?");
                                let display = if unnamed {
                                    format!("<unnamed: {root_path}>")
                                } else {
                                    name.to_string()
                                };
                                println!(
                                    "    - {display} [instance: {instance}] ({note_count} notes, last indexed: {last_indexed})"
                                );
                            } else {
                                println!(
                                    "    - {name} ({note_count} notes, last indexed: {last_indexed})"
                                );
                            }
                            // nw-366. Only on a POSITIVE count. `null` means
                            // the note scan came back short, and a deficit
                            // derived from a partial read is not a deficit —
                            // that is `note_count`'s own rule one line above.
                            if let Some(deficit) = v
                                .get("notes_predating_frontmatter_indexing")
                                .and_then(|value| value.as_u64())
                                .filter(|count| *count > 0)
                            {
                                println!(
                                    "{}",
                                    frontmatter_backfill_warning(
                                        v["root_path"].as_str().unwrap_or("<this vault>"),
                                        deficit,
                                    )
                                );
                            }
                        }
                    }
                    // nw-249(a): `unwrap_or(0)` collapsed a DELIBERATE null.
                    //
                    // The emitter sets these to `null` when the count could
                    // not be READ, and `brain_status`'s own description says:
                    // "a count of `null` means it could NOT BE READ, which is
                    // NOT zero and is not a reason to re-index. Check
                    // `unavailable` (and `counts_complete`) before acting on
                    // any count." This render did exactly what that sentence
                    // forbids — printed `Notes: 0`, which reads as "empty, go
                    // re-index", for a vault that is merely unreadable.
                    //
                    // The MCP client can inspect the null itself; a human
                    // reading the text render cannot. So this surface is the
                    // one that most needs the disclosure, not the least.
                    let count = |key: &str| -> String { render_optional_count(value.get(key)) };
                    println!("  Notes:     {}", count("notes"));
                    println!("  Headings:  {}", count("headings"));
                    println!("  Sections:  {}", count("sections"));
                    println!("  Tags:      {}", count("tags"));
                    println!("  Wikilinks: {}", count("wikilinks"));
                    println!("  Repos:     {}", count("repo_count"));
                    if let Some(skipped) = value.get("skipped_notes") {
                        let skipped_count =
                            skipped.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                        if skipped_count > 0 {
                            println!("  Skipped notes: {skipped_count}");
                            if let Some(paths) = skipped.get("paths").and_then(|v| v.as_array()) {
                                for path in paths.iter().filter_map(|p| p.as_str()) {
                                    println!("    - {path}");
                                }
                            }
                            if skipped
                                .get("truncated")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                println!("    (list truncated)");
                            }
                        }
                        // nw-653: see the typed render above.
                        let pending = skipped
                            .get("reconciliation_pending")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        if pending > 0 {
                            println!("  Watcher reconciliation pending: {pending} file(s)");
                            for note in skipped
                                .get("reconciliation_pending_notes")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                            {
                                let field = |key: &str| {
                                    note.get(key).and_then(|v| v.as_str()).unwrap_or_default()
                                };
                                println!("    - {}: {}", field("path"), field("reason"));
                            }
                        }
                        // nw-585: see the typed render above.
                        let unparsed = skipped
                            .get("frontmatter_unparsed")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        if unparsed > 0 {
                            println!(
                                "  Notes indexed without frontmatter (unparsable YAML): {unparsed}"
                            );
                            for note in skipped
                                .get("frontmatter_unparsed_notes")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                            {
                                let field = |key: &str| {
                                    note.get(key).and_then(|v| v.as_str()).unwrap_or_default()
                                };
                                println!("    - {}: {}", field("path"), field("reason"));
                            }
                        }
                    }
                    // nw-670 review M3: the same line as the typed render.
                    if let Some(line) = nestweaver_proto::code_links_from_status_json(&value)
                        .as_ref()
                        .and_then(format_code_links_status)
                    {
                        println!("  {line}");
                    }
                    if let Some(line) = nestweaver_proto::cross_repo_links_from_status_json(&value)
                        .as_ref()
                        .and_then(format_cross_repo_links_status)
                    {
                        println!("  {line}");
                    }
                    for line in format_manifest_failures_status(
                        &nestweaver_proto::manifest_failures_from_status_json(&value),
                    ) {
                        println!("  {line}");
                    }
                    for line in vault_derivation_status_lines(&value) {
                        println!("  {line}");
                    }
                    if let Some(near) = value.get("notes_near_size_limit") {
                        let near_count = near.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                        if near_count > 0 {
                            println!("  Notes approaching size limit: {near_count}");
                            if let Some(notes) = near.get("notes").and_then(|v| v.as_array()) {
                                for note in notes {
                                    let path =
                                        note.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                                    let bytes =
                                        note.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                                    println!("    - {path} ({bytes} bytes)");
                                }
                            }
                            if near
                                .get("truncated")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                println!("    (list truncated)");
                            }
                        }
                    }
                    // And name what failed, so the reader is not left to infer
                    // it from which rows say "unavailable".
                    if let Some(unavailable) = value.get("unavailable").and_then(|v| v.as_array())
                        && !unavailable.is_empty()
                    {
                        let names: Vec<String> = unavailable
                            .iter()
                            .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
                            .collect();
                        println!(
                            "  NOTE: {} count(s) could not be read ({}). These are NOT zero, \
                             and re-indexing is not the remedy.",
                            names.len(),
                            names.join(", ")
                        );
                    }
                    // One-line publication state when dirty — the "(see
                    // warning)" pointer is only printed when a warning
                    // actually follows (wedged), not for a routine in-flight
                    // publication.
                    if let Some(publication) = value.get("index_publication") {
                        let dirty = publication["dirty"].as_bool().unwrap_or(false);
                        let wedged = publication["wedged"].as_bool().unwrap_or(false);
                        if let Some(line) = format_index_publication_line(dirty, wedged) {
                            println!("{line}");
                        }
                    }
                    if let Some(embedding) = value.get("embedding_status") {
                        println!("Embedding:");
                        println!(
                            "{}",
                            format_embedding_status(&embedding_status_from_json(embedding))
                        );
                    }
                    if let Some(search) =
                        value.get("search_status").filter(|value| !value.is_null())
                    {
                        println!("Search index:");
                        println!("{}", format_search_status(&search_status_from_json(search)));
                    }
                    // Interaction tracking — local check (not in MCP response).
                    // The sidecar is created (empty) by `InteractionTracker::new`
                    // when an MCP/daemon starts with --track-interactions, so:
                    //   - file present + scores > 0  -> "enabled, N scored"
                    //   - file present + scores == 0 -> "enabled (no events yet)"
                    //   - file absent                -> "disabled"
                    match nestweaver_engine::load_interaction_data(db_path) {
                        Some(data) if !data.scores.is_empty() => {
                            println!(
                                "  interaction_tracking: enabled ({} nodes scored)",
                                data.scores.len()
                            );
                        }
                        Some(_) => {
                            println!("  interaction_tracking: enabled (no events recorded yet)");
                        }
                        None => {
                            println!(
                                "  interaction_tracking: disabled (run with --track-interactions to enable)"
                            );
                        }
                    }
                    // Forward structured warnings from the MCP response —
                    // duplicate-vault-root collisions, a wedged index
                    // publication, and any kind this binary predates —
                    // through the shared renderer so nothing is dropped.
                    if let Some(warnings) = value.get("warnings").and_then(|v| v.as_array()) {
                        eprint!("{}", format_brain_status_warnings(warnings));
                    }
                    // ── Upstream server info ─────────────────────────────
                    let upstream_configs = nestweaver_client::discovery::discover_upstreams(
                        db_path.parent().unwrap_or(std::path::Path::new(".")),
                    );
                    if !upstream_configs.is_empty() {
                        println!();
                        for ucfg in &upstream_configs {
                            let name = ucfg.name.as_deref().unwrap_or("upstream");
                            let mode = format!("{:?}", ucfg.mode).to_lowercase();
                            match nestweaver_client::upstream::UpstreamHandle::from_config(ucfg) {
                                Ok(handle) => {
                                    // Try a quick HealthCheck to determine reachability.
                                    let rt = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build()
                                        .unwrap();
                                    let health_result = rt.block_on(async {
                                        let mut client = handle.client();
                                        let mut req = tonic::Request::new(
                                            nestweaver_proto::HealthCheckRequest {},
                                        );
                                        handle.inject_auth(&mut req);
                                        tokio::time::timeout(
                                            std::time::Duration::from_secs(2),
                                            client.health_check(req),
                                        )
                                        .await
                                    });

                                    match health_result {
                                        Ok(Ok(resp)) => {
                                            let version = resp.into_inner().version;
                                            // Try to get repo count.
                                            let repo_count = rt.block_on(async {
                                                let mut client = handle.client();
                                                let mut req = tonic::Request::new(
                                                    nestweaver_proto::RepoStatesRequest {},
                                                );
                                                handle.inject_auth(&mut req);
                                                client
                                                    .repo_states(req)
                                                    .await
                                                    .map(|r| format!("{} repos", r.into_inner().repos.len()))
                                                    .unwrap_or_else(|_| "repository inventory unavailable; check upstream authorization and retry".to_string())
                                            });
                                            println!(
                                                "  Server: {name} (v{version}, {mode} mode, healthy, {repo_count})"
                                            );
                                        }
                                        _ => {
                                            println!("  Server: {name} ({mode} mode, unreachable)");
                                        }
                                    }
                                }
                                Err(_) => {
                                    println!("  Server: {name} ({mode} mode, config error)");
                                }
                            }
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(db_path))?;
            let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;
            let note_count = store.count_notes().map_err(|e| anyhow::anyhow!(e))?;
            let heading_count = store.count_headings().map_err(|e| anyhow::anyhow!(e))?;
            let section_count = store.count_sections().map_err(|e| anyhow::anyhow!(e))?;
            let tag_count = store.count_tags().map_err(|e| anyhow::anyhow!(e))?;
            let wikilink_count = store
                .count_wikilink_edges()
                .map_err(|e| anyhow::anyhow!(e))?;
            let repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;

            /// Resolve last_indexed_at for a vault: prefer the extension-store
            /// timestamp (actual indexer run), fall back to max(note.modified_at)
            /// for databases indexed before this feature was added.
            fn resolve_last_indexed(
                db_path: &Path,
                vault_uid: &str,
                store: &GraphStore,
            ) -> Option<String> {
                if let Some(ts) = get_last_indexed_at(db_path, vault_uid) {
                    return Some(ts);
                }
                // Fallback: max(note.modified_at).
                store
                    .list_notes(Some(vault_uid))
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|n| n.modified_at.clone())
                    .max()
            }

            if json {
                // ONE builder, ONE schema: the direct path serves the same
                // top-level document the daemon serves, with the fields only
                // a live daemon can answer honestly emitted as explicit nulls
                // and the bypass disclosed in-band (`degraded_components` +
                // a `daemon_bypassed` warning + a synthesized `_meta`) — a
                // `--json 2>/dev/null` consumer can never silently receive a
                // different document. The warnings are in-band here, so the
                // stderr warnings block below serves text mode only.
                nestweaver_mcp::tools::set_current_db_path(db_path.to_path_buf());
                let mut value = nestweaver_mcp::tools::brain_status_json(&store, None)?;
                let cause = if use_daemon {
                    last_daemon_bypass_cause()
                        .unwrap_or_else(|| "the daemon did not serve this request".to_string())
                } else {
                    "the daemon was explicitly bypassed (--no-daemon / NESTWEAVER_NO_DAEMON)"
                        .to_string()
                };
                nestweaver_mcp::tools::mark_brain_status_daemon_bypassed(&mut value, &cause);
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok((EXIT_SUCCESS, None));
            } else {
                println!("Brain status:");
                println!("  Database:  {}", db_path.display());
                println!("  Vaults:    {}", vaults.len());
                // When two rows share a name + root_path we surface
                // `instance_id` so the user can tell them apart and target
                // `brain remove --instance <id>` precisely. Empty-named
                // rows (phantom registrations) are always annotated so they
                // don't render as blank lines.
                let mut name_counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for v in &vaults {
                    *name_counts.entry(v.name.as_str()).or_insert(0) += 1;
                }
                for v in &vaults {
                    // nw-269: `unwrap_or_default().len()` is the same defect
                    // one layer earlier — the error never even becomes a null,
                    // it becomes an empty Vec and then a confident `0`. The
                    // MCP route logs the failure and emits null; this route
                    // silently agreed that the vault was empty.
                    // nw-366. The backfill deficit is counted from the notes
                    // this read already returned rather than by a second query,
                    // and it is `None` on the failure path for the same reason
                    // the count is `unavailable` there: a deficit derived from
                    // a read that failed is not a deficit.
                    let (vault_note_count, frontmatter_deficit) =
                        match store.list_notes(Some(&v.uid)) {
                            Ok(notes) => {
                                let deficit = notes
                                    .iter()
                                    .filter(|note| {
                                        nestweaver_store::GraphStore::
                                            note_predates_frontmatter_indexing(note)
                                    })
                                    .count() as u64;
                                (notes.len().to_string(), Some(deficit))
                            }
                            Err(error) => {
                                tracing::warn!(
                                    vault = %v.uid,
                                    "per-vault note count unavailable: {error}"
                                );
                                (render_optional_count(Some(&serde_json::Value::Null)), None)
                            }
                        };
                    let last_indexed = resolve_last_indexed(db_path, &v.uid, &store)
                        .unwrap_or_else(|| "never".to_string());
                    let ambiguous = name_counts.get(v.name.as_str()).copied().unwrap_or(0) > 1;
                    let unnamed = v.name.is_empty();
                    if ambiguous || unnamed {
                        let display = if unnamed {
                            format!("<unnamed: {}>", v.root_path)
                        } else {
                            v.name.clone()
                        };
                        println!(
                            "    - {display} [instance: {}] ({vault_note_count} notes, last indexed: {last_indexed})",
                            v.instance_id
                        );
                    } else {
                        println!(
                            "    - {} ({vault_note_count} notes, last indexed: {last_indexed})",
                            v.name
                        );
                    }
                    if let Some(deficit) = frontmatter_deficit.filter(|count| *count > 0) {
                        println!("{}", frontmatter_backfill_warning(&v.root_path, deficit));
                    }
                }
                println!("  Notes:     {note_count}");
                println!("  Headings:  {heading_count}");
                println!("  Sections:  {section_count}");
                println!("  Tags:      {tag_count}");
                println!("  Wikilinks: {wikilink_count}");
                println!("  Repos:     {}", repos.len());
                // One-line publication state when dirty, mirroring the
                // daemon-routed path — the "(see warning)" pointer only when
                // a warning actually follows (wedged).
                let publication = nestweaver_engine::index_publication::status(db_path);
                if let Some(line) =
                    format_index_publication_line(publication.dirty, publication.is_wedged())
                {
                    println!("{line}");
                }
                // nw-121: the daemon prints an `Embedding:` block here and this
                // path printed nothing at all — same command, same database,
                // silently different answer. An omitted section reads as "no
                // such concept", so a user could not tell that semantic
                // retrieval state was simply unknown. Embedding state is
                // RUNTIME state owned by the daemon (model actually loaded,
                // device actually selected), so this path genuinely cannot
                // report it — but saying so is not the same as saying nothing.
                println!("Embedding:");
                println!(
                    "  State:            unknown (runtime state is held by the daemon; \
                     start it with `nestweaver daemon --db {} start`)",
                    db_path.display()
                );
                // Interaction tracking is opt-in (enabled via `mcp
                // --track-interactions`). InteractionTracker::new touches
                // the sidecar at startup so we can distinguish three states:
                //   - file present + scores > 0  -> enabled, accumulating
                //   - file present + scores == 0 -> enabled, no events yet
                //   - file absent                -> disabled
                match nestweaver_engine::load_interaction_data(db_path) {
                    Some(data) if !data.scores.is_empty() => {
                        println!(
                            "  interaction_tracking: enabled ({} nodes scored)",
                            data.scores.len()
                        );
                    }
                    Some(_) => {
                        println!("  interaction_tracking: enabled (no events recorded yet)");
                    }
                    None => {
                        println!(
                            "  interaction_tracking: disabled (run with --track-interactions to enable)"
                        );
                    }
                }
                // The same line the daemon-routed render prints.
                if let Some(line) =
                    nestweaver_proto::cross_repo_links_from_status_json(&serde_json::json!({
                        "cross_repo_links":
                            nestweaver_engine::cross_repo_links::cross_repo_links_status_json(
                                Some(db_path),
                            ),
                    }))
                    .as_ref()
                    .and_then(format_cross_repo_links_status)
                {
                    println!("  {line}");
                }
            }

            // Forward the SAME structured warnings the daemon-routed path
            // renders — duplicate-vault-root collisions, a wedged index
            // publication, and any kind this binary predates — through the
            // shared builder + renderer. The previous locally-derived block
            // only ever reported duplicate roots, so a wedged publication
            // produced no text output on this path at all.
            let warnings = nestweaver_mcp::tools::brain_status_warnings(&store, Some(db_path));
            eprint!("{}", format_brain_status_warnings(&warnings));

            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::StaleCheck { json, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            // nw-087: read-only command — fail `db_not_found` on a
            // missing --db before any daemon/store connect could create one.
            require_existing_db(&db_path)?;

            // ── daemon guard ──────────────────────────────────────
            if let Some(value) = try_hybrid_json_rpc_checked(
                use_daemon,
                &db_path,
                None,
                "stale_check",
                serde_json::json!({}),
            )? {
                // Emit the exact JSON shape the direct path
                // produces (the hybrid `_meta` envelope — whose background
                // `stale_repos` verdict could contradict the tool's fresh
                // `any_stale` — is replaced by a top-level `stale_repos`
                // computed from the actual stale list).
                let urls_where = |field: &str| -> Vec<serde_json::Value> {
                    value
                        .get("repos")
                        .and_then(|v| v.as_array())
                        .map(|repos| {
                            repos
                                .iter()
                                .filter(|r| r[field].as_bool().unwrap_or(false))
                                .filter_map(|r| r["url"].as_str().map(serde_json::Value::from))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let stale_urls = urls_where("is_stale");
                // nw-163: `stale_repos` narrowed along with `is_stale`, so it
                // no longer names an `incomplete` or `missing` repo — yet those
                // still exit 2, and the CI recipe tells a job to re-index what
                // this array names. `needs_reindex_repos` is the actionable
                // set; `stale_repos` stays behind-HEAD only.
                let needs_reindex_urls = urls_where("needs_reindex");
                // nw-370: read the daemon's OWN per-row verdict rather than
                // recomputing one. The client refuses to talk to a
                // version-mismatched daemon at all — it restarts it (see
                // `NestweaverClient::connect`) — so the daemon serving this
                // reply is always this binary's own `tool_stale_check`, and a
                // client-side fallback here would be dead code pretending to
                // be a safety net.
                let resolver_stale_urls = urls_where("resolver_stale");
                let any_stale = value
                    .get("any_stale")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // Falls back to `any_stale` only for an older daemon that does
                // not send the field — never silently reports "nothing to do".
                let any_needs_reindex = value
                    .get("any_needs_reindex")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(any_stale);

                if json {
                    let normalized = serde_json::json!({
                        "repo_count": value.get("repo_count").cloned().unwrap_or(serde_json::json!(0)),
                        "any_stale": any_stale,
                        "any_needs_reindex": any_needs_reindex,
                        "stale_repos": stale_urls,
                        "needs_reindex_repos": needs_reindex_urls,
                        "resolver_stale_repos": resolver_stale_urls,
                        "repos": value.get("repos").cloned().unwrap_or_else(|| serde_json::json!([])),
                    });
                    println!("{}", serde_json::to_string_pretty(&normalized)?);
                } else {
                    let repo_count = value
                        .get("repo_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let repos = value
                        .get("repos")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    if repos.is_empty() {
                        println!("No repos indexed.");
                    } else {
                        println!(
                            "Stale check: {} repo(s), {}",
                            repo_count,
                            // Gated on the union, not `any_stale`: an `incomplete`
                            // repo exits 2, and a banner reading "up to date"
                            // above that exit code is the exact dishonesty this
                            // contract exists to remove.
                            if any_needs_reindex {
                                "NEEDS REINDEX"
                            } else {
                                "up to date"
                            }
                        );
                        for r in &repos {
                            println!("{}", stale_check_row_line(r));
                        }
                    }
                }
                // nw-370: the remedy, on stderr in BOTH modes — the same
                // channel and the same renderer `hubs`/`bridges` use, so the
                // migration instruction cannot drift between the command that
                // detects the condition and the commands that suffer from it.
                // A `--json` gate reads `resolver_stale_repos`; stdout stays
                // pure JSON either way.
                //
                // This route knows the denominator (`repo_count` is on the
                // payload), unlike the `hubs` daemon route, so it passes
                // `Some` rather than printing a floor.
                let resolver_stale_names: Vec<String> = resolver_stale_urls
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if let Some(note) = nestweaver_engine::resolver_generation::staleness_note_for(
                    &resolver_stale_names,
                    value
                        .get("repo_count")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as usize),
                ) {
                    eprintln!("warning: {note}");
                }
                // Stale-check is a freshness gate — exit non-zero when stale.
                //
                // nw-370: generation staleness reuses EXIT_NEEDS_REINDEX (2)
                // rather than taking a fifth code. `2` already means "at least
                // one repo needs re-indexing", the remedy is identical, and
                // `docs/ci-integration.md` plus the shipped pre-push hook
                // already gate on it — so every existing CI gate catches the
                // 9.0.0 migration with no edit. A new code would be silently
                // ignored by exactly the gates that need to fire, which is the
                // opposite of what a migration signal is for. Callers that
                // must distinguish the states read `status` /
                // `resolver_stale_repos`, which is what those fields are for.
                return Ok((
                    if any_needs_reindex {
                        EXIT_NEEDS_REINDEX
                    } else {
                        EXIT_SUCCESS
                    },
                    None,
                ));
            }

            let store = open_store(Some(&db_path))?;
            // A freshness GATE that cannot read the database must fail, not
            // report "No repos indexed." and exit 0. The daemon path already
            // propagates this; only the direct path swallowed it.
            let repos = store
                .list_repos(None)
                .map_err(|error| anyhow::anyhow!("list repos: {error}"))?;

            // nw-370: the fourth rung. See `tool_stale_check` — the two
            // routes must answer identically, so this is the same decision
            // made from the same computation.
            //
            // `ResolverStaleness::from_store` is the existing direct-route
            // constructor (`hubs`, `bridges`), which wraps
            // `ResolverGenerations::stale_repos`, the sole computation
            // (nw-358). No fourth decider is added here; this reads the same
            // verdict and joins it to the rows by uid.
            let resolver_stale: std::collections::HashSet<String> =
                ResolverStaleness::from_store(&store, &db_path)
                    .stale_repos
                    .into_iter()
                    .collect();

            let mut any_stale = false;
            let mut any_needs_reindex = false;
            let mut results: Vec<serde_json::Value> = Vec::new();

            for repo in &repos {
                // A local working tree that no longer exists on disk is
                // unverifiable — flag it `[missing]` and count it as stale
                // instead of silently reporting `[ok]`.
                let local_missing = repo
                    .local_root()
                    .map(|p| !std::path::Path::new(p).exists())
                    .unwrap_or(false);

                // nw-266: the `else` here was a hardcoded `None`, while the
                // MCP route asked the REMOTE. `local_root()` is `None`
                // whenever `root_path` is empty and the url is not `file://`,
                // which is exactly the case `remote_head` exists to serve — so
                // the CLI reported `ok` and exited 0 for a repo the daemon
                // called stale, and a CI gate built on it passed.
                //
                // Third divergence in this function pair, after nw-163
                // (`is_stale`) and nw-256 (`commits_behind`). Both of those
                // fixes left a comment saying the routes must not drift, and
                // the next divergence appeared underneath it — so the decision
                // now lives in one place both routes CALL.
                let current_head = nestweaver_engine::repo_head::current_head(
                    local_missing,
                    repo.local_root(),
                    &repo.url,
                );

                let is_valid_sha = nestweaver_engine::repo_head::is_full_sha(&repo.indexed_sha);
                // nw-256: `Option<u64>`, mirroring `tool_stale_check`. The
                // note directly below is about `is_stale` diverging between
                // these two routes for exactly this reason; #310 fixed the
                // daemon side of the COUNT and left this one on
                // `unwrap_or(0)`, so the same defect recurred in the same
                // function pair. A failed `git rev-list` means "could not
                // count", and this branch is only reached when HEAD already
                // differs from the indexed SHA — so a zero here is a
                // self-contradiction ("stale, 0 commits behind"), not an
                // answer.
                let commits_behind: Option<u64> = match (&current_head, repo.local_root()) {
                    (Some(head), Some(path)) if is_valid_sha && *head != repo.indexed_sha => {
                        nestweaver_engine::repo_head::commits_between(path, &repo.indexed_sha, head)
                    }
                    _ => Some(repo.staleness_commits_behind as u64),
                };
                // nw-163: BEHIND HEAD and nothing else — a deleted working
                // tree is `status: "missing"` and `needs_reindex: true`, not
                // "stale". The daemon path (`tool_stale_check`) was changed to
                // this and the direct path was not, so the same repo answered
                // `is_stale: true` without a daemon and `false` with one.
                let is_stale = match &current_head {
                    Some(head) => head != &repo.indexed_sha,
                    // The working tree is GONE, so HEAD is unknowable and
                    // "behind HEAD" cannot be asserted. The stored counter is
                    // a leftover from the last successful check; reporting it
                    // as staleness presents a stale guess as a fact. `status`
                    // says "missing" and `needs_reindex` is true, which is the
                    // actionable truth.
                    None if local_missing => false,
                    // An uncountable distance is not a claim of zero: if HEAD
                    // is unknown AND the stored counter cannot be read,
                    // staleness is simply not assertable here.
                    None => commits_behind.is_some_and(|behind| behind > 0),
                };
                // A repo whose SHA was committed but whose content never
                // landed (interrupted index) compares equal to HEAD yet
                // serves an empty graph — flag it stale so the gate catches
                // it. Mirrors the daemon path's `stale_check` tool; errors
                // propagate (a gate that cannot answer must fail).
                let content_missing = store
                    .repo_index_incomplete(repo)
                    .map_err(|e| anyhow::anyhow!("repo_index_incomplete: {e}"))?;
                let empty_complete = store
                    .repo_index_empty_complete(repo)
                    .map_err(|e| anyhow::anyhow!("repo_index_empty_complete: {e}"))?;

                // nw-163: `is_stale` means BEHIND HEAD and nothing else; the
                // actionable union lives in `needs_reindex`. Mirrors
                // `tool_stale_check` exactly — the two paths must not drift.
                //
                // nw-370: `outdated_resolver` sits BELOW the three git-derived
                // states, matching `tool_stale_check`. `resolver_stale` on the
                // row keeps the fact visible when that precedence reports a git
                // reason instead.
                let repo_resolver_stale = resolver_stale.contains(&repo.uid);
                let status = if local_missing {
                    "missing"
                } else if content_missing {
                    "incomplete"
                } else if empty_complete {
                    "no_indexable_content"
                } else if is_stale {
                    "stale"
                } else if repo_resolver_stale {
                    "outdated_resolver"
                } else {
                    "ok"
                };
                let needs_reindex = status != "ok" && status != "no_indexable_content";
                if is_stale {
                    any_stale = true;
                }
                if needs_reindex {
                    any_needs_reindex = true;
                }

                results.push(serde_json::json!({
                    // nw-634: `uid`/`root_path`/`name`/`display_name`,
                    // matching `tool_stale_check` exactly — same fields, same
                    // resolver (`nestweaver_engine::repo_display_name`), so a
                    // caller cannot tell which route answered.
                    "uid": repo.uid,
                    "root_path": repo.root_path,
                    "name": repo.name,
                    "display_name": nestweaver_engine::repo_display_name(repo),
                    "url": repo.url,
                    "indexed_sha": repo.indexed_sha,
                    "current_head": current_head,
                    "is_stale": is_stale,
                    // nw-370: independent of `is_stale`, matching
                    // `tool_stale_check`.
                    "resolver_stale": repo_resolver_stale,
                    "needs_reindex": needs_reindex,
                    "staleness_commits_behind": commits_behind,
                    "status": status,
                }));
            }

            if json {
                // Include the actual stale list so `any_stale: true`
                // never sits next to an empty stale set.
                let urls_where = |field: &str| -> Vec<serde_json::Value> {
                    results
                        .iter()
                        .filter(|r| r[field].as_bool().unwrap_or(false))
                        .filter_map(|r| r["url"].as_str().map(serde_json::Value::from))
                        .collect()
                };
                let stale_urls = urls_where("is_stale");
                let needs_reindex_urls = urls_where("needs_reindex");
                // nw-370: NOT folded into `stale_repos` — that key means
                // behind-HEAD here and generation-stale on `hub_nodes`, and
                // merging the two populations under one name is how those
                // surfaces would start contradicting each other.
                let resolver_stale_urls = urls_where("resolver_stale");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "repo_count": repos.len(),
                        "any_stale": any_stale,
                        "any_needs_reindex": any_needs_reindex,
                        "stale_repos": stale_urls,
                        "needs_reindex_repos": needs_reindex_urls,
                        "resolver_stale_repos": resolver_stale_urls,
                        "repos": results,
                    }))?
                );
            } else if repos.is_empty() {
                println!("No repos indexed.");
            } else {
                println!(
                    "Stale check: {} repo(s), {}",
                    repos.len(),
                    // Gated on the union, not `any_stale`: an `incomplete`
                    // repo exits 2, and a banner reading "up to date"
                    // above that exit code is the exact dishonesty this
                    // contract exists to remove.
                    if any_needs_reindex {
                        "NEEDS REINDEX"
                    } else {
                        "up to date"
                    }
                );
                for r in &results {
                    println!("{}", stale_check_row_line(r));
                }
            }
            // nw-370: same note, same renderer, same stream as the daemon
            // route above. This route holds the store, so the denominator is
            // exact.
            let resolver_stale_names: Vec<String> = results
                .iter()
                .filter(|r| r["resolver_stale"].as_bool().unwrap_or(false))
                .filter_map(|r| r["url"].as_str().map(String::from))
                .collect();
            if let Some(note) = nestweaver_engine::resolver_generation::staleness_note_for(
                &resolver_stale_names,
                Some(repos.len()),
            ) {
                eprintln!("warning: {note}");
            }
            // Stale-check is a freshness gate — exit non-zero when stale.
            // nw-370: see the daemon route for why generation staleness reuses
            // EXIT_NEEDS_REINDEX rather than taking a code of its own.
            Ok((
                if any_needs_reindex {
                    EXIT_NEEDS_REINDEX
                } else {
                    EXIT_SUCCESS
                },
                None,
            ))
        }

        BrainCommands::Watch {
            path,
            name,
            instance,
            db,
            ignore,
            refresh_wiki_hours,
            config,
            force,
        } => {
            if refresh_wiki_hours.is_some() && config.is_none() {
                // nw-252: a USAGE error, so EXIT_USAGE (64, EX_USAGE from
                // sysexits.h) — the same code clap's own parse failures
                // return. Exiting 1 made "you invoked this wrong" and "the
                // operation failed" indistinguishable to a caller.
                eprintln!("Error: --refresh-wiki-hours requires --config");
                return Ok((EXIT_USAGE, None));
            }
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            if !path.exists() || !path.is_dir() {
                eprintln!("Error: vault path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            // `is_dir()` only needs `stat(2)`, which succeeds on a `chmod 000`
            // directory — that needs `+x` on the PARENT, not on the directory
            // itself — so the guard above passes on a vault that cannot be
            // ENUMERATED. Probing `read_dir` is the cheapest predicate that
            // actually tests the operation the refresh depends on (nw-287).
            if let Err(err) = std::fs::read_dir(&path) {
                eprintln!(
                    "Error: vault directory cannot be read: {} ({err})",
                    path.display()
                );
                return Ok((EXIT_ERROR, None));
            }
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
            // Resolve and validate the same instance precedence as brain add,
            // brain refresh, top-level index, and top-level watch.
            let instance_id = resolve_instance_id_for_db(instance, config.as_deref(), &db_path)?;
            let instance_cfg = load_instance_config_opt(config.as_deref());
            let note_limits = instance_cfg
                .as_ref()
                .map(|c| c.indexing.note_limits())
                .unwrap_or_default();

            // nw-273: same refusal as `watch` — see the comment there for why
            // this is refused rather than restored in-process.
            if let Some(hours) = refresh_wiki_hours {
                eprintln!(
                    "Error: --refresh-wiki-hours is not implemented (requested {hours}h).\n\
                     It scheduled nothing on the daemon route, and since the watcher took \
                     its write lease it schedules nothing on the direct route either — \
                     while reporting that it had.\n\
                     Run the refresh on your own schedule instead:\n  \
                     nestweaver materialize-projects --config <path>"
                );
                return Ok((EXIT_USAGE, None));
            }

            // Respect watch config when --config is provided.
            // nw-673: taken before `watch_cfg` consumes the config.
            let cross_domain = instance_cfg
                .as_ref()
                .map(|config| config.cross_domain.clone())
                .unwrap_or_default();
            let watch_cfg = instance_cfg.map(|c| c.watch).unwrap_or_default();
            if !watch_cfg.enabled {
                out.status(
                    "Watching disabled in instance config ([watch] enabled = false). Exiting.",
                );
                return Ok((EXIT_SUCCESS, None));
            }

            let extra_patterns = parse_ignore_flag(&ignore);

            if use_daemon {
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ))?;
                // Absolute path: the daemon runs with CWD=/ (would watch the wrong dir).
                let vault_abs = abs_for_daemon(&path);
                let req = nestweaver_proto::WatchVaultRequest {
                    force,
                    vault_path: vault_abs.to_string_lossy().to_string(),
                    vault_name: vault_name.clone(),
                    instance_id: instance_id.clone(),
                    extra_ignore_patterns: extra_patterns.clone(),
                    max_note_bytes: rpc_max_note_bytes(config.as_deref(), note_limits),
                };
                let resp = rt.block_on(async {
                    client
                        .inner_mut()
                        .watch_vault(req)
                        .await
                        .map(|r| r.into_inner())
                        .map_err(daemon_status_error)
                })?;
                if !resp.ok {
                    eprintln!("Error: {}", resp.message);
                    return Ok((EXIT_ERROR, None));
                }
                out.status(&format!(
                    "Watching {} via daemon (Ctrl-C to stop)",
                    path.display()
                ));

                // Block until Ctrl-C or daemon death, then send StopWatch.
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = ctrlc_handler(move || {
                    let _ = tx.send(());
                });

                wait_for_daemon_watcher(&rt, &mut client, resp.watcher_id, &rx)?;

                stop_owned_daemon_watcher(&rt, &mut client, resp.watcher_id)?;
                out.status("Watcher stopped.");
                return Ok((EXIT_SUCCESS, None));
            }

            // nw-267 (sibling): `brain watch` runs the same direct-watcher
            // fallback as `watch` and had the same hole — a long-running
            // writer with no write lease. Fixing one route and leaving its
            // twin is the exact pattern this batch exists to stop, and the
            // PID lock file written below is a HINT to readers, not a lock:
            // it cannot survive PID reuse and an operator's `rm` erases it.
            //
            // Held for the whole watch.
            let write_lease = require_exclusive_store_access(&db_path, "brain watch")?;

            let tantivy_sidecar = tantivy_sidecar_path_for(&db_path);
            let manifests_path = nestweaver_engine::manifest_cache_path(&db_path);
            let wiki_instance_id = instance_id.clone();
            let watcher = BrainWatcher::new(&db_path, &path, instance_id, vault_name)
                .with_tantivy_index(&tantivy_sidecar)
                .with_manifests_path(&manifests_path)
                .with_extra_ignore_patterns(&extra_patterns)
                .with_note_limits(note_limits)
                .with_cross_domain_config(cross_domain)
                .with_debounce_ms(watch_cfg.debounce_ms);
            let stop = watcher.shutdown_handle();

            // Write a PID lock file so MCP servers and other readers know a
            // watcher is active and should open the database read-only.
            let lock_path = {
                let mut s = db_path.as_os_str().to_owned();
                s.push(".lock");
                std::path::PathBuf::from(s)
            };
            let _ = std::fs::write(&lock_path, std::process::id().to_string());

            // Wire Ctrl-C → shutdown_handle.stop(). Best-effort; if the
            // signal handler can't install we still run, the user just
            // has to kill the process.
            let stop_signal = stop.clone();
            let _ = ctrlc_handler(move || stop_signal.stop());

            // Spawn periodic wiki refresh thread if --refresh-wiki-hours
            // is set. The thread sleeps for N hours, calls
            // materialize_projects to re-fetch wiki sources, then loops
            // until the shutdown handle signals stop.
            if let (Some(hours), Some(config_path)) = (refresh_wiki_hours, config.as_deref()) {
                let wiki_db = db_path.clone();
                let wiki_config_path = config_path.to_path_buf();
                let wiki_stop = stop.clone();
                let wiki_instance = wiki_instance_id;
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Runtime::new() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("wiki refresh: failed to create runtime: {e}");
                            return;
                        }
                    };
                    let interval = std::time::Duration::from_secs(hours * 3600);
                    loop {
                        // Sleep in small increments so we notice shutdown quickly.
                        let deadline = std::time::Instant::now() + interval;
                        while std::time::Instant::now() < deadline {
                            if wiki_stop.is_stopped() {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(5));
                        }
                        if wiki_stop.is_stopped() {
                            return;
                        }
                        tracing::info!("periodic wiki refresh triggered");
                        match rt.block_on(async {
                            let mut client = nestweaver_client::DaemonClient::connect(
                                &wiki_db,
                                Some(wiki_config_path.as_path()),
                            )
                            .await?;
                            let mut stream = client
                                .materialize_projects(
                                    wiki_config_path.to_string_lossy().as_ref(),
                                    &wiki_instance,
                                )
                                .await?;
                            let mut last_msg = String::new();
                            while let Some(progress) = stream.message().await? {
                                last_msg = progress.message;
                            }
                            Ok::<_, anyhow::Error>(last_msg)
                        }) {
                            Ok(msg) => tracing::info!("wiki refresh complete: {msg}"),
                            Err(e) => tracing::warn!("wiki refresh failed: {e}"),
                        }
                    }
                });
            }

            out.status(&format!(
                "Watching {} -> {} (Ctrl-C to stop)",
                path.display(),
                db_path.display()
            ));
            if let Err(e) = watcher.run_with_write_lease(&write_lease) {
                // nw-684 review: EVERY failure removes the PID hint written
                // above (e.g. a watcher refusing an unreadable
                // `.brainignore`), or readers keep treating the database as
                // watched by a process that has exited.
                let _ = std::fs::remove_file(&lock_path);
                // A lock failure here means another process (usually a
                // live daemon) holds the DB — name the remedy.
                let msg = format!("{e:#}");
                if let Some(hint) = watch_lock_hint(&msg, &db_path) {
                    eprintln!("Error: watcher: {msg}\nhint: {hint}");
                    return Ok((EXIT_ERROR, None));
                }
                return Err(e).context("watcher");
            }

            // BrainWatcher::run() drops its GraphStore when it returns,
            // which triggers lbug's internal cleanup. However, `launchctl
            // unload` sends SIGKILL after a short grace period (~5 s) if
            // the process hasn't exited. A small sleep here gives the OS
            // time to flush any remaining WAL pages to disk after the
            // store is dropped inside `run()`.
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Clean up the lock file on orderly shutdown.
            let _ = std::fs::remove_file(&lock_path);
            out.status("Watcher stopped.");
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Refresh {
            path,
            name,
            instance,
            db,
            config,
            since,
            ignore,
            json,
            fail_on_skip,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            if !path.exists() || !path.is_dir() {
                eprintln!("Error: vault path is not a directory: {}", path.display());
                return Ok((EXIT_ERROR, None));
            }
            // `is_dir()` only needs `stat(2)`, which succeeds on a `chmod 000`
            // directory — that needs `+x` on the PARENT, not on the directory
            // itself — so the guard above passes on a vault that cannot be
            // ENUMERATED. Probing `read_dir` is the cheapest predicate that
            // actually tests the operation the refresh depends on (nw-287).
            if let Err(err) = std::fs::read_dir(&path) {
                eprintln!(
                    "Error: vault directory cannot be read: {} ({err})",
                    path.display()
                );
                return Ok((EXIT_ERROR, None));
            }
            let vault_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vault")
                    .to_string()
            });
            let extra_patterns = parse_ignore_flag(&ignore);
            let canonical = abs_for_daemon(&path);
            let note_limits = note_limits_from_config(config.as_deref())?;
            // Parse before registration discovery: an invalid timestamp must
            // not autostart a daemon or touch graph/runtime state.
            let since_time = since
                .as_deref()
                .map(|value| {
                    parse_iso8601_to_system_time(value).with_context(|| {
                        format!(
                            "invalid --since timestamp '{}': expected ISO 8601 (e.g. 2026-05-26T00:00:00Z)",
                            value
                        )
                    })
                })
                .transpose()?;
            // nw-684 Task 5c: on the direct path (`--no-daemon`) refuse over
            // an unloadable `.brainignore` before registration discovery opens
            // the store and before the write lease is taken, mirroring the
            // `brain add` pre-check. The daemon routes check it themselves.
            if !use_daemon {
                nestweaver_engine::load_brain_ignore(&path, &extra_patterns)?;
            }

            // nw-098: resolve the instance from any EXISTING registration for this
            // root before falling back to flag > config > "default".
            //
            // `resolve_instance_id` alone made a refresh with no `--instance`
            // land on the literal "default" and register a SECOND vault for a
            // root already registered under another instance. The root hash is
            // identical in both UIDs, so the tool had everything it needed to
            // know it was the same vault, and forked it anyway: note counts
            // became the SUM and `brain search` started returning duplicate
            // rows. The command documented in this repo's own CLAUDE.md did
            // this.
            let existing =
                vault_registrations_for_root(use_daemon, &db_path, config.as_deref(), &canonical)?;
            let explicit = explicit_instance_id(instance.as_deref(), config.as_deref());
            let instance_id = match existing.as_slice() {
                // Nothing registered here yet — a genuine create.
                [] => resolve_instance_id_for_db(instance, config.as_deref(), &db_path)?,
                [(registered, uid)] => match &explicit {
                    // An explicit instance that disagrees with the registration
                    // is a mistake, not an instruction to fork. Name both and
                    // refuse.
                    Some(asked) if asked != registered => {
                        eprintln!(
                            "Error: {} is already registered under instance '{registered}' ({uid}), \n\
                             but --instance/--config asked for '{asked}'.\n\
                             Refusing to create a second vault for the same root — that splits \n\
                             note counts and makes `brain search` return duplicate rows.\n\
                             help: re-run with --instance {registered} to refresh it in place, \n\
                             or use a different root path.",
                            canonical.display()
                        );
                        return Ok((EXIT_ERROR, None));
                    }
                    // Adopt the registration. This is the case the bug broke:
                    // the caller expressed no intent, so refresh what is there.
                    _ => registered.clone(),
                },
                // Already forked (this is the nw-098 damage state). An
                // explicit instance that names one of them is actionable;
                // anything else would be a guess that compounds the fork.
                many => {
                    let names = many
                        .iter()
                        .map(|(inst, _)| inst.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    match &explicit {
                        Some(asked) if many.iter().any(|(inst, _)| inst == asked) => asked.clone(),
                        Some(asked) => {
                            eprintln!(
                                "Error: {} is registered under {} instances ({names}), and \
                                 --instance/--config asked for '{asked}', which is not one of them.\n\
                                 help: pass --instance with one of: {names}",
                                canonical.display(),
                                many.len()
                            );
                            return Ok((EXIT_ERROR, None));
                        }
                        None => {
                            eprintln!(
                                "Error: {} is registered under {} instances ({names}).\n\
                                 Refusing to guess which to refresh.\n\
                                 help: pass --instance with one of: {names}",
                                canonical.display(),
                                many.len()
                            );
                            return Ok((EXIT_ERROR, None));
                        }
                    }
                }
            };

            // Compute vault UID for recording last_indexed_at.
            let v_uid = nestweaver_schema::vault_uid(&instance_id, &canonical.to_string_lossy());

            if use_daemon {
                // nw-196: the terminal `Done` event carries the skip rows the
                // `--json` payload and `--fail-on-skip` are built from.
                let mut terminal: Option<nestweaver_proto::IndexProgress> = None;
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt.block_on(nestweaver_client::DaemonClient::connect(
                    &db_path,
                    config.as_deref(),
                ))?;
                if let Some(since_time) = since_time {
                    let since_unix_seconds = since_time
                        .duration_since(std::time::UNIX_EPOCH)
                        .context("--since timestamp predates the Unix epoch")?
                        .as_secs();
                    let req = nestweaver_proto::RefreshVaultSinceRequest {
                        vault_path: canonical.to_string_lossy().to_string(),
                        vault_name: vault_name.clone(),
                        extra_ignore_patterns: extra_patterns.clone(),
                        instance_id: instance_id.to_string(),
                        since_unix_seconds,
                        max_note_bytes: rpc_max_note_bytes(config.as_deref(), note_limits),
                    };
                    rt.block_on(async {
                        let stream = client
                            .inner_mut()
                            .refresh_vault_since(req)
                            .await
                            .map_err(|status| {
                                if status.code() == tonic::Code::Unimplemented {
                                    anyhow::anyhow!(
                                        "the running daemon does not support incremental vault refresh; upgrade/restart it and retry (refusing full or direct-store fallback)"
                                    )
                                } else {
                                    anyhow::anyhow!(
                                        "incremental vault refresh RPC failed (refusing direct-store fallback): {status}"
                                    )
                                }
                            })?
                            .into_inner();
                        consume_cli_index_progress(stream, |progress| {
                            let phase_name = match progress.phase {
                                5 => "Done",
                                6 => "Error",
                                _ => "Progress",
                            };
                            eprintln!("[{phase_name}] {}", progress.message);
                            if progress.phase == nestweaver_proto::Phase::Done as i32 {
                                terminal = Some(progress.clone());
                            }
                        })
                        .await
                    })?;
                } else {
                    // Route full refresh through daemon's IndexVault RPC.
                    let req = nestweaver_proto::IndexVaultRequest {
                        // Absolute path: the daemon runs with CWD=/ and would otherwise
                        // resolve a client-relative vault path against the wrong directory.
                        vault_path: canonical.to_string_lossy().to_string(),
                        vault_name: vault_name.clone(),
                        extra_ignore_patterns: extra_patterns.clone(),
                        instance_id: instance_id.to_string(),
                        max_note_bytes: rpc_max_note_bytes(config.as_deref(), note_limits),
                    };
                    rt.block_on(async {
                        let stream = client.inner_mut().index_vault(req).await?.into_inner();
                        consume_cli_index_progress(stream, |progress| {
                            let phase_name = match progress.phase {
                                5 => "Done",
                                6 => "Error",
                                _ => "Progress",
                            };
                            eprintln!("[{phase_name}] {}", progress.message);
                            if progress.phase == nestweaver_proto::Phase::Done as i32 {
                                terminal = Some(progress.clone());
                            }
                        })
                        .await
                    })?;
                }
                let terminal = terminal.ok_or_else(|| {
                    anyhow::anyhow!("vault refresh completed without a terminal progress payload")
                })?;
                let skipped: Vec<RefreshSkipRow> = terminal
                    .skipped_files
                    .iter()
                    .map(RefreshSkipRow::from_wire)
                    .collect();
                let unparsed: Vec<RefreshSkipRow> = terminal
                    .frontmatter_unparsed
                    .iter()
                    .map(RefreshSkipRow::from_wire)
                    .collect();
                return finish_vault_refresh(
                    json,
                    fail_on_skip,
                    &vault_name,
                    since.is_some(),
                    &terminal.message,
                    &skipped,
                    &unparsed,
                );
            }

            // Both refresh arms below write the graph and rebuild Tantivy on
            // the direct path.
            let write_lease = require_exclusive_store_access(&db_path, "refresh a vault")?;
            // (summary text, skip rows, frontmatter-unparsed rows), rendered by
            // `finish_vault_refresh` once the link and search rebuilds ran.
            let refresh_outcome: (String, Vec<RefreshSkipRow>, Vec<RefreshSkipRow>);
            let incremental = since.is_some();

            if let Some(since_str) = since {
                // Incremental refresh: only re-index files modified since the
                // given timestamp.
                let since_time = since_time.expect("parsed above when --since is present");
                let result =
                    index_markdown_directory_since_with_ignore_and_write_lease_and_note_limits(
                        &path,
                        &db_path,
                        &instance_id,
                        &vault_name,
                        since_time,
                        &extra_patterns,
                        note_limits,
                        &write_lease,
                    )
                    .context("index_markdown_directory_since")?;
                require_complete_graph_publication(
                    "incremental vault refresh",
                    &result.publication,
                )?;

                // Record the indexer run timestamp.
                if let Err(e) = record_last_indexed_at(&db_path, &v_uid) {
                    tracing::warn!("failed to record last_indexed_at: {e}");
                }

                let mut message = format!(
                    "Incremental refresh of vault '{}' (since {}): \
                     checked {} file(s), updated {} note(s), dropped {} prior note(s), \
                     {} heading(s), {} section(s), {} tag(s), \
                     {} wikilink edge(s) on changed notes.",
                    result.vault_name,
                    since_str,
                    result.files_checked,
                    result.notes_updated,
                    result.notes_deleted,
                    result.headings_count,
                    result.sections_count,
                    result.tags_count,
                    result.changed_note_link_edges,
                );
                // nw-694: the skip rows themselves, as the daemon route and
                // the full-refresh summary list them, so a gap names its path.
                for row in &result.skipped {
                    message.push_str(&format!("\n  {} - {}", row.path, row.reason));
                }
                // nw-585: the same lines the daemon route appends.
                if let Some(unparsed) = nestweaver_engine::index_md::frontmatter_unparsed_summary(
                    &result.frontmatter_unparsed,
                ) {
                    message.push('\n');
                    message.push_str(&unparsed);
                }
                if !json {
                    println!("{message}");
                }
                refresh_outcome = (
                    message,
                    result
                        .skipped
                        .iter()
                        .map(RefreshSkipRow::from_engine)
                        .collect(),
                    result
                        .frontmatter_unparsed
                        .iter()
                        .map(RefreshSkipRow::from_engine)
                        .collect(),
                );
            } else {
                // Full refresh: the markdown indexer's writable store performs
                // the old-vault cascade and replacement in one transaction.
                // Its returned delete count is therefore committed truth; a
                // failed cascade/write propagates and cannot be reported as a
                // successful dropped note.
                let result =
                    index_markdown_directory_with_ignore_and_deletion_count_and_write_lease_and_note_limits(
                        &path,
                        &db_path,
                        &instance_id,
                        &vault_name,
                        &extra_patterns,
                        note_limits,
                        &write_lease,
                    )
                    .context("index_markdown_directory")?;
                require_complete_graph_publication("full vault refresh", &result.publication)?;

                // Record the indexer run timestamp.
                if let Err(e) = record_last_indexed_at(&db_path, &v_uid) {
                    tracing::warn!("failed to record last_indexed_at: {e}");
                }

                let message = nestweaver_engine::index_md::format_markdown_refresh_summary(&result);
                if !json {
                    println!("{message}");
                }
                refresh_outcome = (
                    message,
                    result
                        .index
                        .skipped
                        .iter()
                        .map(RefreshSkipRow::from_engine)
                        .collect(),
                    result
                        .index
                        .frontmatter_unparsed
                        .iter()
                        .map(RefreshSkipRow::from_engine)
                        .collect(),
                );
            }

            // nw-675: both arms recreate notes, and the cascade took their
            // code links; rebuild them before returning.
            reconcile_code_links_direct(
                &db_path,
                &write_lease,
                config.as_deref(),
                &format!("refresh of vault {}", path.display()),
            );

            // Auto-populate Tantivy BM25 index after brain refresh so that
            // `brain search` works immediately without a manual reindex.
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            match TantivyIndex::open_or_create(&tantivy_path) {
                Ok(tantivy) => {
                    let store_for_tantivy =
                        GraphStore::open_read_only_with_authority(&db_path, &write_lease)?;
                    match tantivy.reindex_from_store(&store_for_tantivy) {
                        // Stdout carries only the JSON payload under --json.
                        Ok(count) if json => eprintln!("Tantivy: indexed {count} document(s)"),
                        Ok(count) => println!("Tantivy: indexed {count} document(s)"),
                        Err(e) => tracing::warn!("Tantivy reindex failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("Tantivy open failed: {e}"),
            }

            let (message, skipped, unparsed) = refresh_outcome;
            finish_vault_refresh(
                json,
                fail_on_skip,
                &vault_name,
                incremental,
                &message,
                &skipped,
                &unparsed,
            )
        }

        BrainCommands::Remove {
            path,
            instance,
            db,
            config,
        } => {
            // The SAME two helpers `brain add` and `brain refresh` use, so all
            // three resolve a pinned config identically. Hand-rolling
            // `db.unwrap_or_else(default_db_path)` and `unwrap_or("default")`
            // here is what made this command ignore a config its siblings honour.
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let instance_specified = instance.is_some() || config.is_some();
            // An unscoped removal intentionally cleans all discovered rows at
            // this path. It need not choose an identity for a new mutation.
            let instance_id_owned = resolve_instance_id(instance, config.as_deref())?;
            let instance_id = instance_id_owned.as_str();

            let canonical = abs_for_daemon(&path);
            let canon_str = canonical.to_string_lossy();
            let raw_str = path.to_string_lossy();
            let v_uid_canon = nestweaver_schema::vault_uid(instance_id, &canon_str);
            let v_uid_raw = nestweaver_schema::vault_uid(instance_id, &raw_str);

            // Fetch vault list via daemon RPC (preferred) or direct store open (fallback).
            let fetch_vaults =
                |inst_filter: Option<&str>| -> anyhow::Result<Vec<nestweaver_schema::Vault>> {
                    let mut args = serde_json::json!({});
                    if let Some(inst) = inst_filter {
                        args["instance"] = serde_json::json!(inst);
                    }
                    if let Some(value) = try_hybrid_json_rpc_checked(
                        use_daemon,
                        &db_path,
                        config.as_deref(),
                        "list_vaults",
                        args,
                    )? {
                        // Was `.unwrap_or_default()`. This list decides WHICH
                        // vault gets removed, so a decode failure turning into
                        // an empty list means the command reports "no vault
                        // found" and exits successfully, having done nothing —
                        // for a destructive command, silently doing nothing on
                        // an error is its own kind of wrong answer.
                        serde_json::from_value(unwrap_hybrid_payload(value))
                            .context("decode vault list from daemon")
                    } else {
                        // Accidental daemon-unavailable fallback stays closed,
                        // and `--config` still cannot be honored by the direct
                        // store. The honored CI direct route (`use_daemon ==
                        // false` without a pinned config) is selected before
                        // that guard, matching list_projects.
                        if use_daemon || config.is_some() {
                            ensure_direct_store_fallback_allowed(&db_path, config.as_deref())?;
                        }
                        let store = GraphStore::open_read_only(&db_path).with_context(|| {
                            format!("open {} to list vaults", db_path.display())
                        })?;
                        store.list_vaults(inst_filter).map_err(Into::into)
                    }
                };

            // Helper: a stored vault matches the caller's path if any of its
            // representations (canonical, literal, tilde-expanded `~`)
            // resolve to the same absolute path. `brain add` may have
            // registered the vault with a literal `~/...` string (from a
            // config file or programmatic call) while the caller of
            // `brain remove` typically passes a shell-expanded absolute
            // path. A naive `vault_uid` lookup misses these cases even
            // though `brain status` clearly shows the row.
            let path_matches = |stored: &str| -> bool {
                if stored == "~" || stored.starts_with("~/") {
                    let Ok(expanded) = nestweaver_engine::resolve_user_path(stored) else {
                        return false;
                    };
                    if expanded.to_string_lossy() == *canon_str {
                        return true;
                    }
                    return std::fs::canonicalize(&expanded)
                        .is_ok_and(|path| path.to_string_lossy() == *canon_str);
                }
                if stored == canon_str || stored == raw_str {
                    return true;
                }
                let stored_canon = std::fs::canonicalize(stored)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| stored.to_string());
                if stored_canon == *canon_str {
                    return true;
                }
                false
            };

            // If the caller passed `--instance`, treat it as a precise
            // selector. If the direct vault_uid lookup misses (path
            // stored under a non-canonical form like a literal `~/...`),
            // fall back to a list-scan scoped to the requested instance
            // before failing.
            //
            // If `--instance` is absent, fall back to the historical
            // ghost-row cleanup behavior: remove the default-UID row
            // plus any other row whose canonical root_path matches.
            let mut uids_to_remove: Vec<String> = Vec::new();
            if instance_specified {
                // Check if the direct UID exists in the vault list
                let instance_vaults = fetch_vaults(Some(instance_id))?;
                let has_canon = instance_vaults.iter().any(|v| v.uid == v_uid_canon);
                let has_raw = instance_vaults.iter().any(|v| v.uid == v_uid_raw);
                if has_canon {
                    uids_to_remove.push(v_uid_canon);
                } else if has_raw {
                    uids_to_remove.push(v_uid_raw);
                } else {
                    for v in &instance_vaults {
                        if path_matches(&v.root_path) {
                            uids_to_remove.push(v.uid.clone());
                        }
                    }
                }
                if uids_to_remove.is_empty() {
                    eprintln!(
                        "Error: no vault with instance '{instance_id}' found at {canon_str}.\n  \
                         Run `nestweaver brain status` to see registered vaults and their instance ids,\n  \
                         then re-run with the correct --instance (or omit --instance to clean up every row at this path)."
                    );
                    return Ok((EXIT_NOT_FOUND, None));
                }
            } else {
                let all_vaults = fetch_vaults(None)?;
                for v in &all_vaults {
                    if path_matches(&v.root_path) && !uids_to_remove.contains(&v.uid) {
                        uids_to_remove.push(v.uid.clone());
                    }
                }
            }

            if uids_to_remove.is_empty() {
                // nw-587: no graph row, but the database may still remember
                // registering a vault here (one a WAL move-aside dropped).
                // Removing it on purpose means forgetting that too, or
                // `brain status` reports it as lost forever. `brain status`
                // prints this command as the way to say "dropped on purpose".
                // An unreadable sidecar must not turn a plain not-found into
                // an error: log it and fall through (`brain status` keeps
                // disclosing it until the next record rewrites the file).
                let forgotten = match nestweaver_engine::vault_registration::forget_root(
                    &db_path, &canonical,
                ) {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::warn!("nw-587: vault registrations not consulted: {error:#}");
                        0
                    }
                };
                if forgotten > 0 {
                    println!(
                        "No vault in the graph at {canon_str}; forgot {forgotten} registration(s) \
                         it had lost."
                    );
                    return Ok((EXIT_SUCCESS, None));
                }
                println!("No vault found at {canon_str}; 0 row(s) cleaned.");
                return Ok((EXIT_NOT_FOUND, None));
            }

            let mut vault_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("vault")
                .to_string();
            // Resolve vault name from the daemon vault list instead of direct store access
            let all_known_vaults = fetch_vaults(None)?;
            for uid in &uids_to_remove {
                if let Some(v) = all_known_vaults.iter().find(|v| v.uid == *uid) {
                    vault_name.clone_from(&v.name);
                }
            }

            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            let mut total_dropped = 0usize;
            let mut rows_cleaned = 0usize;
            let mut reconciliation_failures: Vec<String> = Vec::new();
            for uid in &uids_to_remove {
                match rt.block_on(client.remove_vault(uid)) {
                    Ok(resp) => {
                        if resp.committed {
                            total_dropped += resp.notes_deleted as usize;
                            rows_cleaned += 1;
                        }
                        // The daemon already rebuilt the search index as part
                        // of the removal, and already reports whether that
                        // succeeded. Its answer is the one to relay.
                        reconciliation_failures.extend(
                            resp.reconciliation_failures
                                .iter()
                                .map(|failure| failure.message.clone()),
                        );
                    }
                    Err(e) => {
                        eprintln!("Error: failed to remove vault '{uid}': {e}.");
                        return Ok((EXIT_ERROR, None));
                    }
                }
            }
            // The rebuild is the DAEMON's, and this command always routes
            // through it — `DaemonClient::connect` above autostarts one.
            //
            // nw-242: this used to open Tantivy from the CLI and rebuild it
            // here. That could never work: the daemon it just talked to holds
            // tantivy's INDEX_WRITER_LOCK for its lifetime, so
            // `open_or_create` was refused every time, and the refusal was
            // reported as "the search index could NOT be rebuilt … brain
            // search will keep returning notes from this vault" — on EVERY
            // SUCCESSFUL removal, for work the daemon had already done
            // (`rebuild_tantivy_after_mutation` in `remove_vault`).
            //
            // The concern behind it was right — a stale index after a REMOVE
            // keeps RETURNING notes from a dropped vault, which is worse than
            // the misses a stale index after an ADD causes. It was already
            // handled one layer down, and I did not check before adding a
            // second writer.
            if reconciliation_failures.is_empty() {
                println!(
                    "Removed vault '{vault_name}' ({total_dropped} note(s) dropped, \
                     {rows_cleaned} row(s) cleaned). Search index rebuilt."
                );
            } else {
                println!(
                    "Removed vault '{vault_name}' ({total_dropped} note(s) dropped, \
                     {rows_cleaned} row(s) cleaned).\n\
                     WARNING: {} post-commit step(s) failed, so `brain search` may \
                     keep returning notes from this vault. Run \
                     `nestweaver brain reindex-search` to clear them:\n  {}",
                    reconciliation_failures.len(),
                    reconciliation_failures.join("\n  ")
                );
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::ReindexSearch { db } => {
            let db_path = db.unwrap_or_else(default_db_path);

            if use_daemon {
                match tokio::runtime::Runtime::new() {
                    Ok(rt) => {
                        let connect =
                            rt.block_on(nestweaver_client::DaemonClient::connect(&db_path, None));
                        match connect {
                            Ok(mut client) => {
                                let rpc = rt.block_on(async {
                                    client
                                        .inner_mut()
                                        .reindex_search(nestweaver_proto::ReindexSearchRequest {})
                                        .await
                                        .map(|r| r.into_inner())
                                });
                                match rpc {
                                    Ok(resp) => {
                                        let sidecar = tantivy_sidecar_path_for(&db_path);
                                        println!(
                                            "Tantivy reindex complete: {} document(s) at {} (via daemon)",
                                            resp.document_count,
                                            sidecar.display()
                                        );
                                        return Ok((EXIT_SUCCESS, None));
                                    }
                                    Err(status) => {
                                        ensure_direct_store_fallback_allowed(&db_path, None)
                                            .with_context(|| {
                                                format!(
                                                    "daemon reindex RPC failed ({}); refusing direct fallback",
                                                    status.message()
                                                )
                                            })?;
                                        eprintln!(
                                            "warning: daemon reindex RPC failed ({}); falling back to direct mode",
                                            status.message()
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                ensure_direct_store_fallback_allowed(&db_path, None).with_context(
                                    || {
                                        format!(
                                            "daemon search-index connection failed ({error:#}); refusing direct fallback"
                                        )
                                    },
                                )?;
                            }
                        }
                    }
                    Err(error) => {
                        ensure_direct_store_fallback_allowed(&db_path, None).with_context(|| {
                            format!(
                                "create runtime for daemon search-index rebuild failed ({error}); refusing direct fallback"
                            )
                        })?;
                    }
                }
            }

            // `open_store` below is READ-ONLY, but `open_or_create` on the
            // sidecar is a WRITE, and Tantivy enforces a single writer via its
            // own `INDEX_WRITER_LOCK`. Against a daemon that holds it this
            // either fails with `DirectoryLockBusy` or — because that lock is
            // released whenever the daemon's writer is dropped — succeeds and
            // races it on segment and meta writes.
            //
            // The database lock is the right gate even though the contended
            // resource is the sidecar: the daemon takes the database lock for
            // its whole life, so "nobody holds the database" is what makes the
            // derived index safe to rewrite. Guarding on Tantivy's own lock
            // would only narrow the window, not close it.
            // Held across the Tantivy rebuild. This is the site the probe could not
            // protect: `open_store` below is read-only, so nothing else holds the
            // database while the sidecar is rewritten, and any client connect
            // autostarts a daemon.
            let write_lease = require_exclusive_store_access(&db_path, "rebuild the search index")?;

            let sidecar = tantivy_sidecar_path_for(&db_path);
            let store = GraphStore::open_read_only_with_authority(&db_path, &write_lease)?;
            let idx = TantivyIndex::open_or_create(&sidecar)
                .with_context(|| format!("open tantivy at {}", sidecar.display()))?;
            let count = idx
                .reindex_from_store(&store)
                .with_context(|| "reindex Tantivy from store")?;
            println!(
                "Tantivy reindex complete: {count} document(s) at {}",
                sidecar.display()
            );
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Search {
            query: raw_query,
            limit,
            json,
            db,
            config,
            prf,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_existing_db(&db_path)?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 20);

            // Route through the daemon's typed `Search` RPC when running. The
            // daemon owns the writer-mode Tantivy index and shares dispatch
            // with the MCP server (`tool_brain_search`), so daemon-routed
            // searches eliminate the "Database is locked" reader fallback
            // and stay in sync with live re-indexing. The direct-disk
            // implementation below is only the honored CI direct route
            // (`use_daemon == false`). An unavailable daemon never falls
            // through; `ensure_direct_store_fallback_allowed` refuses it.
            if use_daemon {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => Some(runtime),
                    Err(error) => {
                        ensure_direct_store_fallback_allowed(&db_path, config.as_deref())
                            .with_context(|| {
                                format!(
                                    "create runtime for brain search failed ({error}); refusing direct fallback"
                                )
                            })?;
                        None
                    }
                };
                if let Some(rt) = rt {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    match rt.block_on(nestweaver_client::hybrid::HybridClient::connect(
                        &db_path,
                        config.as_deref(),
                        &cwd,
                    )) {
                        Ok(mut hybrid) if hybrid.has_upstreams() => {
                            let params = serde_json::json!({
                                "query": raw_query,
                                "limit": limit,
                                "prf": prf,
                            });
                            match rt.block_on(hybrid.query("brain_search", &params)) {
                                Ok(result) => {
                                    if json {
                                        println!("{}", serde_json::to_string_pretty(&result)?);
                                    } else {
                                        render_brain_search_json(&result)?;
                                    }
                                    let count = result
                                        .get("results")
                                        .and_then(|v| v.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(0);
                                    let stats = format!(
                                        "{} results in {} (via daemon+hybrid)",
                                        count,
                                        format_elapsed(t0.elapsed())
                                    );
                                    return Ok((EXIT_SUCCESS, Some(stats)));
                                }
                                Err(error) => {
                                    ensure_direct_store_fallback_allowed(
                                        &db_path,
                                        config.as_deref(),
                                    )
                                    .with_context(|| {
                                        format!(
                                            "hybrid brain search failed ({error:#}); refusing direct fallback"
                                        )
                                    })?;
                                    warn_daemon_bypassed(
                                        &db_path,
                                        "brain_search",
                                        &format!("{error:#}"),
                                    );
                                }
                            }
                        }
                        Ok(mut hybrid) => {
                            let req = nestweaver_proto::BrainSearchRequest {
                                query: raw_query.clone(),
                                limit: limit as i32,
                                response_format: None,
                                include_bodies: false,
                                prf,
                                rerank: false,
                                root: None,
                            };
                            match rt.block_on(async {
                                hybrid.inner_mut().search(req).await.map(|r| r.into_inner())
                            }) {
                                Ok(resp) => {
                                    render_brain_search_response(&resp, json)?;
                                    let stats = format!(
                                        "{} results in {} (via daemon)",
                                        resp.results.len(),
                                        format_elapsed(t0.elapsed())
                                    );
                                    return Ok((EXIT_SUCCESS, Some(stats)));
                                }
                                Err(error) => {
                                    ensure_direct_store_fallback_allowed(
                                        &db_path,
                                        config.as_deref(),
                                    )
                                    .with_context(|| {
                                        format!(
                                            "daemon brain search failed ({error:#}); refusing direct fallback"
                                        )
                                    })?;
                                    warn_daemon_bypassed(
                                        &db_path,
                                        "brain_search",
                                        &format!("{error:#}"),
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            ensure_direct_store_fallback_allowed(&db_path, config.as_deref())
                                .with_context(|| {
                                    format!(
                                        "daemon configuration could not be safely honored for brain_search ({error:#}); refusing direct fallback"
                                    )
                                })?;
                            warn_daemon_bypassed(&db_path, "brain_search", &format!("{error:#}"));
                        }
                    }
                }
            }

            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // Feature F7: PRF is enabled by the --prf flag OR `[ranking] enable_prf`.
            let prf_enabled = prf || cfg.as_ref().map(|c| c.ranking.enable_prf).unwrap_or(false);

            // Reuse the canonical MCP search implementation in-process so the
            // direct CLI, daemon, and MCP share counted search pages, logical
            // grouping, identity rules, and ranking behavior. This performs no
            // network I/O and uses the already-open direct-disk store/index.
            nestweaver_mcp::tools::set_current_db_path(db_path.clone());
            nestweaver_mcp::tools::set_current_instance_config(cfg.map(std::sync::Arc::new));
            let response = nestweaver_mcp::tools::dispatch(
                &store,
                tantivy.as_ref(),
                "brain_search",
                serde_json::json!({
                    "query": raw_query,
                    "limit": limit,
                    "prf": prf_enabled,
                }),
                None,
            );
            nestweaver_mcp::tools::set_current_instance_config(None);
            let response = response?;

            if json {
                print_json_payload(&response)?;
            } else {
                render_brain_search_json(&response)?;
            }
            let result_count = response
                .get("returned_matches")
                .and_then(|value| value.as_u64())
                .unwrap_or_else(|| {
                    response
                        .get("results")
                        .and_then(|value| value.as_array())
                        .map_or(0, |results| results.len() as u64)
                });
            let stats = format!(
                "{} results in {}",
                result_count,
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::Context {
            seeds,
            token_budget,
            limit,
            json,
            db,
            config: config_path,
            kinds,
            repos,
            vaults,
            path_prefix,
            tags,
            exclude_tags,
            weight_ppr,
            weight_bm25,
            weight_semantic,
            since,
            recency_weight,
            recency_half_life_days,
            inline_bodies,
            root,
            prf,
            rerank,
            intent,
            no_tests,
            prefer_instance,
            no_embed,
        } => {
            let db_path = resolve_db_with_config(db, config_path.as_deref())?;
            let cfg = load_instance_config_opt(config_path.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 30);

            // Parse the optional --intent override into a `QueryIntent`.
            // Surface invalid values as a CLI error rather than silently
            // ignoring (mirrors `nestweaver context --intent`).
            let parsed_intent: Option<QueryIntent> = intent
                .as_deref()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid --intent value: {e}"))?;

            // Route through daemon's GetContext RPC when available and no
            // flags require direct-disk processing. `--no-tests` and
            // `--prefer-instance` are applied client-side and the daemon
            // proto does not yet carry them, so fall through to the local
            // path when either is set.
            if use_daemon && !no_tests && prefer_instance.is_none() {
                // Build params JSON — used by both hybrid and typed paths.
                let context_params = serde_json::json!({
                        "seeds": seeds,
                        "token_budget": token_budget.unwrap_or(0),
                        "limit": limit,
                        "repos": repos,
                        "vaults": vaults,
                        "kinds": kinds,
                        "path_prefix": path_prefix.clone().unwrap_or_default(),
                        "tags": tags,
                        "exclude_tags": exclude_tags,
                        // nw-670 re-review F3: null when unset, so an
                        // explicit `--weight-ppr 0` is not mistaken for it.
                        "weight_ppr": weight_ppr,
                        "weight_bm25": weight_bm25,
                        "intent": intent.clone().unwrap_or_default(),
                        "include_seeds": true,
                        "include_bodies": inline_bodies,
                        // nw-340: an absent `--root` used to be sent as `""`,
                        // which the daemon joins onto a repo-relative path and
                        // so resolves against its OWN cwd — the same wrong-tree
                        // read as omitting the key. Resolve it client-side.
                        "root": client_source_root(root.as_deref()),
                        "prf": prf,
                        "rerank": rerank,
                        "weight_semantic": if no_embed { 0.0 } else { weight_semantic.unwrap_or(0.0) },
                        // nw-295: see the twin in `project-context` — an
                        // absent filter is omitted rather than sent as `""`.
                        "recency_weight": recency_weight,
                        "recency_half_life_days": recency_half_life_days,
                });
                let mut context_params = context_params;
                if !no_embed && weight_semantic.is_none() {
                    context_params
                        .as_object_mut()
                        .unwrap()
                        .remove("weight_semantic");
                }
                if let Some(since) = since.as_deref().filter(|s| !s.is_empty()) {
                    context_params["since"] = serde_json::json!(since);
                }

                let context_response = match try_hybrid_json_rpc_checked(
                    true,
                    &db_path,
                    config_path.as_deref(),
                    "brain_context",
                    context_params,
                ) {
                    Err(error)
                        if error
                            .chain()
                            .any(|cause| cause.to_string().contains("Ambiguous")) =>
                    {
                        return Ok((report_context_lookup_failure(&error, json, &seeds), None));
                    }
                    Err(error)
                        if error
                            .chain()
                            .any(|cause| cause.to_string().contains("No seeds resolved")) =>
                    {
                        return Ok((report_brain_context_not_found(&error, json, &seeds)?, None));
                    }
                    other => other?,
                };
                if let Some(result_json) = context_response {
                    if payload_is_ambiguous(&result_json) {
                        let label = seeds.first().map(String::as_str).unwrap_or("seed");
                        return report_ambiguous_name_payload(label, &result_json, json);
                    }
                    let source = hybrid_source_label(&result_json);
                    // Read the daemon's disclosure BEFORE `from_value` narrows
                    // the payload to the fields `BrainContextResult` declares —
                    // `total`, `truncated`, `truncated_by` and `_meta` are not
                    // among them, and dropping them here is what made a capped
                    // answer on this route indistinguishable from a complete one.
                    let upstream = UpstreamContextDisclosure::from_wire(&result_json)
                        .with_local_publication(&db_path);
                    let result: nestweaver_engine::BrainContextResult =
                        serde_json::from_value(result_json)?;
                    let cut = match token_budget {
                        // nw-316: `false` preserves TODAY's cost for this route. Its
                        // renderer emits full nodes, so the detailed rate is the
                        // correct one here — unlike `project-context`, which
                        // renders concise and was charging this rate anyway.
                        Some(budget) => {
                            limit.min(token_budgeted_truncate(&result.connected, budget, false))
                        }
                        None => limit.min(result.connected.len()),
                    };
                    if json {
                        print_brain_context_json(&result, cut, token_budget, &upstream)?;
                    } else {
                        print_brain_context_text(&result, cut, token_budget, &upstream);
                    }
                    let node_count = result.seeds.len() + cut;
                    let stats = format!(
                        "{} nodes in {} (via {})",
                        node_count,
                        format_elapsed(t0.elapsed()),
                        source,
                    );
                    return Ok((EXIT_SUCCESS, Some(stats)));
                }
            }

            let store = open_store(Some(&db_path))?;
            if rerank {
                store.require_verified_embedding_identity().context(
                    "verify the database embedding identity before direct context reranking",
                )?;
            }
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();

            // Parse the instance config once (when supplied) and reuse it for
            // both Feature F8 ([response]) and Feature F6 ([ranking]).
            let instance_cfg = load_instance_config_opt(config_path.as_deref());
            // Feature F8: response tuning comes from [response] in the instance
            // config when one is supplied; otherwise the built-in defaults.
            let response_config = instance_cfg
                .as_ref()
                .map(|c| c.response.clone())
                .unwrap_or_default();
            // Feature F6: per-path ranking priors. None → no-op below.
            let ranking_config = instance_cfg
                .as_ref()
                .map(|c| c.ranking.clone())
                .filter(|r| !r.is_empty());

            // Feature F7: PRF is enabled by the --prf flag OR `[ranking] enable_prf`.
            let prf_enabled = prf
                || instance_cfg
                    .as_ref()
                    .map(|c| c.ranking.enable_prf)
                    .unwrap_or(false);

            // RFC #6: build custom HybridSearchConfig from optional CLI flags.
            // Finding #7: thread `[seed_resolution]` (with backward-compat
            // shim for legacy `[ranking].test_path_patterns`) from the
            // instance config into the search config so user overrides reach
            // `search_symbols_by_name` at seed resolution.
            let defaults = HybridSearchConfig::default();
            let configured_seed_resolution =
                instance_cfg.as_ref().map(|c| c.seed_resolution.clone());
            let config = HybridSearchConfig {
                weight_ppr: weight_ppr.unwrap_or(defaults.weight_ppr),
                weight_bm25: weight_bm25.unwrap_or(defaults.weight_bm25),
                weight_semantic: if no_embed {
                    0.0
                } else {
                    weight_semantic.unwrap_or(defaults.weight_semantic)
                },
                prf: prf_enabled,
                seed_resolution: configured_seed_resolution
                    .unwrap_or_else(|| defaults.seed_resolution.clone()),
                ..defaults
            };

            let aliases = load_alias_sidecar(&db_path);
            // Thread the parsed `--intent` override (if any) into the PPR
            // engine. None → engine auto-detects from seed kinds, matching
            // historical behavior.
            match build_brain_context_hybrid_with_aliases(
                &store,
                &seeds,
                tantivy.as_ref(),
                &config,
                &aliases,
                Some(&db_path),
                parsed_intent,
                None,
                None,
            ) {
                Ok(mut result) => {
                    // Feature F6: apply per-path ranking priors (dampen/boost)
                    // from `[ranking]` in the instance config, if supplied.
                    // Applied AFTER fusion on the final relevance, BEFORE the
                    // sort/truncation below. No config → no-op.
                    if let Some(ranking) = ranking_config.as_ref() {
                        nestweaver_engine::apply_ranking_priors(&mut result.seeds, ranking);
                        nestweaver_engine::apply_ranking_priors(&mut result.connected, ranking);
                        result.connected.sort_by(|a, b| {
                            b.relevance
                                .partial_cmp(&a.relevance)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    }

                    // nw-405: resolve the scope flags to CONCRETE container
                    // UIDs BEFORE filtering anything, mirroring
                    // `tool_brain_context`. Resolution sits outside the
                    // per-list closure so an unresolvable entry is one ERROR
                    // for the command rather than a predicate that silently
                    // matches nothing on both lists — and it is the same
                    // resolver, so this route and the daemon route can no
                    // longer answer differently for one flag value.
                    let repo_scope = if repos.is_empty() {
                        None
                    } else {
                        Some(resolve_repo_filter(&store, &repos)?)
                    };
                    let vault_scope = if vaults.is_empty() {
                        None
                    } else {
                        Some(resolve_vault_filter(&store, &vaults)?)
                    };

                    // RFC #2: apply post-PPR filters when any filter flag was set.
                    let filter_kinds_lower: Vec<String> =
                        kinds.iter().map(|k| k.to_lowercase()).collect();
                    let apply_filters = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                        if !filter_kinds_lower.is_empty() {
                            nodes.retain(|n| {
                                let kind_lower = n.kind.to_lowercase();
                                filter_kinds_lower
                                    .iter()
                                    .any(|k| kind_lower.starts_with(k.as_str()))
                            });
                        }
                        if let Some(ref repo_uids) = repo_scope {
                            retain_nodes_in_repos(nodes, repo_uids);
                        }
                        if let Some(ref vault_uids) = vault_scope {
                            retain_nodes_in_vaults(nodes, vault_uids);
                        }
                        if let Some(ref prefix) = path_prefix {
                            retain_nodes_under_path_prefix(nodes, prefix.as_str());
                        }
                    };
                    apply_filters(&mut result.seeds);
                    apply_filters(&mut result.connected);

                    // `--no-tests`: drop rows whose location matches any
                    // configured seed-resolution path rule (prefix or
                    // suffix). Distinct from the soft deboost the ranking
                    // pass already applied — this removes the rows entirely
                    // so a strict-prod caller never sees them.
                    if no_tests {
                        let rules = &config.seed_resolution.path_deboost;
                        if !rules.is_empty() {
                            let is_test_path = |loc: &str| -> bool {
                                let lower = loc.to_lowercase();
                                rules.iter().any(|r| match (&r.prefix, &r.suffix) {
                                    (Some(prefix), None) => {
                                        let needle = prefix.trim_start_matches('/').to_lowercase();
                                        !needle.is_empty() && lower.contains(&needle)
                                    }
                                    (None, Some(suffix)) => loc.ends_with(suffix.as_str()),
                                    _ => false,
                                })
                            };
                            let drop_tests = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                                nodes.retain(|n| !is_test_path(&n.location));
                            };
                            drop_tests(&mut result.seeds);
                            drop_tests(&mut result.connected);
                        }
                    }

                    // `--prefer-instance <id>`: scope ranking to a single
                    // instance_id. UIDs encode the owning instance as a
                    // delimited segment: `note:vlt:<inst>:<hash>:...`,
                    // `sym:repo:<inst>:<hash>:...`, `repo:<inst>:<hash>`,
                    // etc. Matching on `:<inst>:` is robust to a partially-
                    // merged DB where some symbol UIDs still encode the
                    // pre-merge repo UID — both the old and new forms still
                    // carry an `:<inst>:` segment that uniquely identifies
                    // the instance, and the leading and trailing colons
                    // prevent accidental substring collisions with hashes.
                    if let Some(ref target) = prefer_instance {
                        let needle = format!(":{target}:");
                        let filter_inst = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|n| n.uid.contains(needle.as_str()));
                        };
                        filter_inst(&mut result.seeds);
                        filter_inst(&mut result.connected);
                    }

                    // tags filter: keep only note/section nodes tagged with any of these.
                    //
                    // nw-407: Symbol nodes used to be kept UNCONDITIONALLY
                    // here ("no tag concept for code"), which made `--tags` an
                    // EXPANSION rather than a filter. Measured on the live
                    // graph at one seed and budget: with `--tags security` the
                    // result was n=71 of which 70 were Symbols and ONE was a
                    // tagged Note; without it, n=30 with 22 of 30 vault
                    // content. Adding the filter GREW the result 30 -> 71 and
                    // drove the tagged share from 22/30 to 1/71.
                    //
                    // The budget leg is why this is not cosmetic: the
                    // pass-through happens before the token budget is spent,
                    // so untagged symbols eat the budget the tagged notes were
                    // asked for. "No tag concept for code" means a symbol
                    // cannot SATISFY a tag scope, not that it is exempt from
                    // one — the old comment documented the mechanism and never
                    // the consequence.
                    if !tags.is_empty() {
                        let tagged_notes = store
                            .list_note_uids_with_tags(&tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let tagged_sections = store
                            .list_section_uids_with_tags(&tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_tagged = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            retain_tagged_nodes(nodes, &tagged_notes, &tagged_sections);
                        };
                        filter_tagged(&mut result.seeds);
                        filter_tagged(&mut result.connected);
                    }

                    // exclude_tags filter: remove note/section nodes tagged with any of these.
                    if !exclude_tags.is_empty() {
                        let excluded_notes = store
                            .list_note_uids_with_tags(&exclude_tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let excluded_sections = store
                            .list_section_uids_with_tags(&exclude_tags)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_excluded = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                !excluded_notes.contains(&item.uid)
                                    && !excluded_sections.contains(&item.uid)
                            });
                        };
                        filter_excluded(&mut result.seeds);
                        filter_excluded(&mut result.connected);
                    }

                    // since filter: hard filter Note/Section nodes by modified_at.
                    if let Some(ref since_ts) = since {
                        let recent_notes = store
                            .list_note_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let recent_sections = store
                            .list_section_uids_modified_since(since_ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let filter_since = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                            nodes.retain(|item| {
                                if item.kind.to_lowercase().contains("symbol") {
                                    return true;
                                }
                                recent_notes.contains(&item.uid)
                                    || recent_sections.contains(&item.uid)
                            });
                        };
                        filter_since(&mut result.seeds);
                        filter_since(&mut result.connected);
                    }

                    // recency bias: soft boost based on note modified_at age.
                    if recency_weight > 0.0 {
                        apply_recency_bias_cli(
                            &store,
                            &mut result.connected,
                            recency_weight,
                            recency_half_life_days,
                        );
                        apply_recency_bias_cli(
                            &store,
                            &mut result.seeds,
                            recency_weight,
                            recency_half_life_days,
                        );
                    }

                    // Feature F17: rerank the top-N retrieved candidates. OFF by
                    // default → byte-identical output. Applied AFTER fusion +
                    // F6 priors + filters, BEFORE truncation. The default scorer
                    // is a transparent monotonic heuristic (NOT a validated nDCG
                    // win); an optional `<db>.rerank.json` learned-weights file
                    // is used if present and version-matched. Reranking only
                    // reorders an already-retrieved set; recall is unchanged.
                    if rerank {
                        let reranker = nestweaver_engine::select_reranker(Some(&db_path));
                        nestweaver_engine::rerank(
                            &mut result.connected,
                            reranker.as_ref(),
                            &store,
                            nestweaver_engine::RERANK_DEFAULT_TOP_N,
                        );
                    }

                    // Feature F8: embed high-relevance bodies inline when the
                    // caller opted in. Off by default → output unchanged.
                    if inline_bodies {
                        let root = root.clone().unwrap_or_else(|| {
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                        });
                        nestweaver_engine::populate_inline_bodies(
                            &store,
                            &mut result.connected,
                            &root,
                            response_config.inline_body_threshold,
                            response_config.inline_max_body_tokens,
                            token_budget,
                            // Local CLI reads from the working tree; no bare-clone
                            // resolver, so bodies come from the FilesystemReader.
                            None,
                        );
                    }

                    // Apply both the count and token caps; neither discards the other.
                    let cut = match token_budget {
                        // nw-316: `false` preserves TODAY's cost for this route. Its
                        // renderer emits full nodes, so the detailed rate is the
                        // correct one here — unlike `project-context`, which
                        // renders concise and was charging this rate anyway.
                        Some(budget) => {
                            limit.min(token_budgeted_truncate(&result.connected, budget, false))
                        }
                        None => limit.min(result.connected.len()),
                    };
                    let node_count = result.seeds.len() + cut;
                    // Direct route: `result.connected` is the pre-cut list this
                    // process built, so there is no upstream to defer to and the
                    // local length IS the honest total.
                    let upstream =
                        UpstreamContextDisclosure::default().with_local_publication(&db_path);
                    if json {
                        print_brain_context_json(&result, cut, token_budget, &upstream)?;
                    } else {
                        print_brain_context_text(&result, cut, token_budget, &upstream);
                    }
                    let stats = format!("{} nodes in {}", node_count, format_elapsed(t0.elapsed()));
                    Ok((EXIT_SUCCESS, Some(stats)))
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Ambiguous") {
                        Ok((report_context_lookup_failure(&e, json, &seeds), None))
                    } else if msg.contains("No seeds resolved") {
                        Ok((report_brain_context_not_found(&e, json, &seeds)?, None))
                    } else {
                        eprintln!("Error: {msg}");
                        Ok((EXIT_ERROR, None))
                    }
                }
            }
        }

        BrainCommands::BrokenLinks {
            max_suggestions,
            limit,
            offset,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            if let Some(value) = try_hybrid_json_rpc_checked(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_broken_links",
                serde_json::json!({
                    "max_suggestions": max_suggestions,
                    "limit": limit,
                    "offset": offset,
                }),
            )? {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else if let Some(arr) = value.get("broken_links") {
                    let links: Vec<nestweaver_engine::BrokenLink> =
                        serde_json::from_value(arr.clone())?;
                    if links.is_empty() {
                        println!("No broken or ambiguous wikilinks found.");
                    } else {
                        // nw-097 class: the daemon reports `total`; this path
                        // printed only how many it chose to show, so 50 of 778
                        // read as "778 does not exist". The direct path below
                        // already renders "N of total" — match it.
                        let total = value.get("total").and_then(|v| v.as_u64());
                        match total {
                            Some(tot) if tot > links.len() as u64 => {
                                println!("Broken / ambiguous wikilinks ({} of {tot}):", links.len())
                            }
                            _ => println!("Broken / ambiguous wikilinks ({}):", links.len()),
                        }
                        // Population counts from the envelope; the page is a
                        // sample and cannot answer the question (nw-297). A
                        // pre-nw-297 daemon omits the fields — fall back to the
                        // page rather than printing nothing.
                        let page_unresolved = links.iter().filter(|l| l.is_unresolved()).count();
                        let unresolved = value
                            .get("unresolved")
                            .and_then(|v| v.as_u64())
                            .map(|n| n as usize)
                            .unwrap_or(page_unresolved);
                        let low_confidence = value
                            .get("low_confidence")
                            .and_then(|v| v.as_u64())
                            .map(|n| n as usize)
                            .unwrap_or(links.len() - page_unresolved);
                        print_link_classification(unresolved, low_confidence);
                        for l in &links {
                            println!(
                                "  [[{}]] in {} (confidence {:.2}) — {}",
                                l.wikilink_text,
                                l.source_path,
                                l.confidence,
                                describe_link_resolution(l)
                            );
                            if !l.suggested_target_uids.is_empty() {
                                print_link_suggestions(l);
                            }
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let all_links = nestweaver_engine::broken_links(&store, max_suggestions)?;
            let total = all_links.len();
            // Classify BEFORE truncating — the page is a sample of a list that
            // is grouped by category, not ranked by severity (nw-297).
            let unresolved = all_links.iter().filter(|l| l.is_unresolved()).count();
            let low_confidence = total - unresolved;
            // nw-341: `total` stays the PRE-offset population on both routes.
            let links: Vec<_> = all_links.into_iter().skip(offset).take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "broken_links": links,
                        "total": total,
                        "returned": links.len(),
                        "truncated": links.len() < total,
                        "offset": offset,
                        "unresolved": unresolved,
                        "low_confidence": low_confidence,
                    }))?
                );
            } else if links.is_empty() {
                println!("No broken or ambiguous wikilinks found.");
            } else {
                if offset > 0 {
                    println!(
                        "Broken / ambiguous wikilinks ({} of {total}, from offset {offset}):",
                        links.len()
                    );
                } else {
                    println!("Broken / ambiguous wikilinks ({} of {total}):", links.len());
                }
                print_link_classification(unresolved, low_confidence);
                for l in &links {
                    println!(
                        "  [[{}]] in {} (confidence {:.2}) — {}",
                        l.wikilink_text,
                        l.source_path,
                        l.confidence,
                        describe_link_resolution(l)
                    );
                    if !l.suggested_target_uids.is_empty() {
                        print_link_suggestions(l);
                    }
                }
            }
            let stats = format!(
                "{} link(s) in {}",
                links.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::Orphans {
            vault,
            path_prefix,
            allow,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            // nw-087: read-only command — fail `db_not_found` on a
            // missing --db, matching the other read commands.
            require_existing_db(&db_path)?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            {
                let mut args = serde_json::json!({});
                if let Some(ref v) = vault {
                    args["vault"] = serde_json::json!(v);
                }
                if let Some(ref p) = path_prefix {
                    args["path_prefix"] = serde_json::json!(p);
                }
                if !allow.is_empty() {
                    args["allowlist"] = serde_json::json!(allow);
                }
                args["limit"] = serde_json::json!(limit);
                if let Some(value) = try_hybrid_json_rpc_checked(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_orphan_documents",
                    args,
                )? {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else if let Some(arr) = value.get("orphans") {
                        let orphans: Vec<nestweaver_engine::OrphanDocument> =
                            serde_json::from_value(arr.clone())?;
                        if orphans.is_empty() {
                            println!("No orphan documents found.");
                        } else {
                            // nw-097 class: daemon carries `total`; printing
                            // only the shown count hid 607 orphans behind 50.
                            let total = value.get("total").and_then(|v| v.as_u64());
                            match total {
                                Some(tot) if tot > orphans.len() as u64 => {
                                    println!("Orphan documents ({} of {tot}):", orphans.len())
                                }
                                _ => println!("Orphan documents ({}):", orphans.len()),
                            }
                            for o in &orphans {
                                println!("  {} — {}", o.title, o.file_path);
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            let all_orphans = nestweaver_engine::orphan_documents(
                &store,
                vault.as_deref(),
                path_prefix.as_deref(),
                &allow,
            )?;
            let total = all_orphans.len();
            let orphans: Vec<_> = all_orphans.into_iter().take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "orphans": orphans,
                        "total": total,
                        "returned": orphans.len(),
                    }))?
                );
            } else if orphans.is_empty() {
                println!("No orphan documents found.");
            } else {
                println!("Orphan documents ({} of {total}):", orphans.len());
                for o in &orphans {
                    println!("  {} — {}", o.title, o.file_path);
                }
            }
            let stats = format!(
                "{} orphan(s) in {}",
                orphans.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::TopicClusters {
            resolution,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            // nw-087: read-only command — fail `db_not_found` on a
            // missing --db, matching the other read commands.
            require_existing_db(&db_path)?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            if let Some(value) = try_hybrid_json_rpc_checked(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_topic_clusters",
                serde_json::json!({ "resolution": resolution, "limit": limit }),
            )? {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else if let Some(arr) = value.get("clusters") {
                    let clusters: Vec<nestweaver_engine::TopicCluster> =
                        serde_json::from_value(arr.clone())?;
                    if clusters.is_empty() {
                        println!("No topic clusters found.");
                    } else {
                        println!("Topic clusters ({}):", clusters.len());
                        for c in &clusters {
                            println!(
                                "  [{}] {} ({} note(s))",
                                c.cluster_id,
                                c.label,
                                c.members.len()
                            );
                        }
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let all_clusters = nestweaver_engine::topic_clusters(&store, resolution)?;
            let total = all_clusters.len();
            let clusters: Vec<_> = all_clusters.into_iter().take(limit).collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "clusters": clusters,
                        "total": total,
                        "returned": clusters.len(),
                    }))?
                );
            } else if clusters.is_empty() {
                println!("No topic clusters found.");
            } else {
                println!("Topic clusters ({} of {total}):", clusters.len());
                for c in &clusters {
                    println!(
                        "  [{}] {} ({} note(s))",
                        c.cluster_id,
                        c.label,
                        c.members.len()
                    );
                }
            }
            let stats = format!(
                "{} cluster(s) in {}",
                clusters.len(),
                format_elapsed(t0.elapsed())
            );
            Ok((EXIT_SUCCESS, Some(stats)))
        }

        BrainCommands::TagGraph {
            tag,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            // nw-087: read-only command — fail `db_not_found` on a
            // missing --db, matching the other read commands.
            require_existing_db(&db_path)?;
            let cfg = load_instance_config_opt(config.as_deref());
            let limit = resolve_limit(limit, cfg.as_ref(), 50);

            {
                let mut args = serde_json::json!({ "limit": limit });
                if let Some(ref t) = tag {
                    args["tag"] = serde_json::json!(t);
                }
                if let Some(value) = try_hybrid_json_rpc_checked(
                    use_daemon,
                    &db_path,
                    config.as_deref(),
                    "brain_tag_graph",
                    args,
                )? {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    } else if tag.is_some() {
                        // Single-tag mode: value is a TagGraph directly.
                        let tg: nestweaver_engine::TagGraph = serde_json::from_value(value)?;
                        println!("#{} — {} note(s)", tg.tag, tg.count);
                        if tg.co_occurring.is_empty() {
                            println!("  no co-occurring tags");
                        } else {
                            println!("  co-occurring:");
                            for c in &tg.co_occurring {
                                println!("    #{} ({})", c.tag, c.count);
                            }
                        }
                    } else if let Some(arr) = value.get("tags") {
                        // All-tags mode.
                        let graphs: Vec<nestweaver_engine::TagGraph> =
                            serde_json::from_value(arr.clone())?;
                        if graphs.is_empty() {
                            println!("no tags");
                        } else {
                            for tg in &graphs {
                                let co = if tg.co_occurring.is_empty() {
                                    "—".to_string()
                                } else {
                                    tg.co_occurring
                                        .iter()
                                        .map(|c| format!("#{} ({})", c.tag, c.count))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                };
                                println!("#{} ({}) → {}", tg.tag, tg.count, co);
                            }
                        }
                    }
                    return Ok((EXIT_SUCCESS, None));
                }
            }

            let store = open_store(Some(&db_path))?;
            match tag {
                Some(tag) => {
                    let tg = nestweaver_engine::tag_graph(&store, &tag)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&tg)?);
                    } else {
                        println!("#{} — {} note(s)", tg.tag, tg.count);
                        if tg.co_occurring.is_empty() {
                            println!("  no co-occurring tags");
                        } else {
                            println!("  co-occurring:");
                            for c in &tg.co_occurring {
                                println!("    #{} ({})", c.tag, c.count);
                            }
                        }
                    }
                }
                None => {
                    let all_graphs = nestweaver_engine::tag_graph_all(&store)?;
                    let total = all_graphs.len();
                    let graphs: Vec<_> = all_graphs.into_iter().take(limit).collect();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "tags": graphs,
                                "total": total,
                                "returned": graphs.len(),
                            }))?
                        );
                    } else if graphs.is_empty() {
                        println!("no tags");
                    } else {
                        for tg in &graphs {
                            let co = if tg.co_occurring.is_empty() {
                                "—".to_string()
                            } else {
                                tg.co_occurring
                                    .iter()
                                    .map(|c| format!("#{} ({})", c.tag, c.count))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            };
                            println!("#{} ({}) → {}", tg.tag, tg.count, co);
                        }
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::DocStats {
            top_tags_limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            // nw-087: read-only command — fail `db_not_found` on a
            // missing --db, matching the other read commands.
            require_existing_db(&db_path)?;

            if let Some(value) = try_hybrid_json_rpc_checked(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_doc_stats",
                serde_json::json!({ "top_tags_limit": top_tags_limit }),
            )? {
                if json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    let stats: nestweaver_engine::DocStats = serde_json::from_value(value)?;
                    for line in doc_stats_text_lines(&stats) {
                        println!("{line}");
                    }
                }
                return Ok((EXIT_SUCCESS, None));
            }

            let store = open_store(Some(&db_path))?;
            let stats = nestweaver_engine::doc_stats(&store, top_tags_limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                for line in doc_stats_text_lines(&stats) {
                    println!("{line}");
                }
            }
            Ok((EXIT_SUCCESS, None))
        }

        BrainCommands::Diff {
            repo,
            since_sha,
            limit,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            require_existing_db(&db_path)?;
            let mut args = serde_json::json!({ "repo": repo });
            if let Some(sha) = since_sha.as_deref() {
                args["since_sha"] = serde_json::json!(sha);
            }
            if let Some(limit) = limit {
                args["limit"] = serde_json::json!(limit);
            }
            // Mirrors `backlinks`/`note get`: daemon route first via the
            // shared JSON-RPC dispatch, falling back to the same
            // `nestweaver_mcp::tools::dispatch` the daemon and MCP routes
            // call, so all three surfaces answer from one implementation.
            //
            // nw-553: an unresolvable `repo` used to fall through the bare `?`
            // to the generic Internal-error path (exit 1, empty stdout). Same
            // catch `blast-radius`/`hubs`/`bridges` use, calling the shared
            // classifier/reporter rather than re-deriving the not-found shape.
            let payload = match try_hybrid_json_rpc_checked(
                use_daemon,
                &db_path,
                config.as_deref(),
                "brain_diff",
                args.clone(),
            ) {
                Err(error) if error_is_unresolved_repo_filter(&error) => {
                    return Ok((report_unresolved_repo_filter(&error, json), None));
                }
                Err(error) => return Err(error),
                Ok(Some(value)) => value,
                Ok(None) => {
                    let store = open_store(Some(&db_path))?;
                    match nestweaver_mcp::tools::dispatch(&store, None, "brain_diff", args, None) {
                        Err(error) if error_is_unresolved_repo_filter(&error) => {
                            return Ok((report_unresolved_repo_filter(&error, json), None));
                        }
                        Err(error) => return Err(error),
                        Ok(value) => value,
                    }
                }
            };
            if json {
                print_json_payload(&payload)?;
            } else {
                println!(
                    "Diff for {}: {} -> {}",
                    payload["repo"].as_str().unwrap_or(&repo),
                    payload["base_sha"].as_str().unwrap_or("?"),
                    payload["head_sha"].as_str().unwrap_or("?")
                );
                if let Some(message) = payload["message"].as_str() {
                    println!("{message}");
                } else {
                    println!(
                        "  {} added, {} modified, {} deleted",
                        payload["files_added"].as_u64().unwrap_or(0),
                        payload["files_modified"].as_u64().unwrap_or(0),
                        payload["files_deleted"].as_u64().unwrap_or(0),
                    );
                    if let Some(symbols) = payload["affected_symbols"].as_array()
                        && !symbols.is_empty()
                    {
                        println!("Affected symbols ({}):", symbols.len());
                        for sym in symbols {
                            println!(
                                "  {} [{}] {}:{}",
                                sym["name"].as_str().unwrap_or("?"),
                                sym["kind"].as_str().unwrap_or("?"),
                                sym["file_path"].as_str().unwrap_or("?"),
                                sym["start_line"].as_u64().unwrap_or(0)
                            );
                        }
                    }
                }
            }
            Ok((EXIT_SUCCESS, None))
        }
    }
}

/// One `brain refresh` skip row, from either route (nw-196).
pub(crate) struct RefreshSkipRow {
    path: String,
    reason_code: String,
    detail: String,
    observed_bytes: Option<u64>,
    limit_bytes: Option<u64>,
    excluded_by_request: bool,
}

impl RefreshSkipRow {
    fn from_wire(row: &nestweaver_proto::IndexSkipDetail) -> Self {
        Self {
            path: row.path.clone(),
            reason_code: row.reason_code.clone(),
            detail: row.detail.clone(),
            observed_bytes: row.observed_bytes,
            limit_bytes: row.limit_bytes,
            excluded_by_request: nestweaver_engine::index_md::skip_wire_excluded_by_request(
                &row.reason_code,
                &row.detail,
            ),
        }
    }

    fn from_engine(row: &nestweaver_engine::index_md::SkippedFile) -> Self {
        Self {
            path: row.path.clone(),
            reason_code: serde_json::to_value(row.reason_code)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "other".to_string()),
            detail: row.reason.clone(),
            observed_bytes: row.observed_bytes,
            limit_bytes: row.limit_bytes,
            excluded_by_request: nestweaver_engine::index_md::skip_excluded_by_request(
                row.reason_code,
                &row.reason,
            ),
        }
    }

    fn to_json(&self, with_request_flag: bool) -> serde_json::Value {
        let mut row = serde_json::json!({
            "path": self.path,
            "reason_code": self.reason_code,
            "detail": self.detail,
            "observed_bytes": self.observed_bytes,
            "limit_bytes": self.limit_bytes,
        });
        if with_request_flag {
            row["excluded_by_request"] = self.excluded_by_request.into();
        }
        row
    }
}

/// Render a finished `brain refresh`'s `--json` payload and pick its exit
/// code (nw-196). The text summary is printed by each route as before.
///
/// `--fail-on-skip` mirrors `index --fail-on-skip` (exit 1), except that a row
/// excluded by request (`.brainignore`) never fails the run: the same shared
/// predicate that fills `excluded_by_request` decides it.
pub(crate) fn finish_vault_refresh(
    json: bool,
    fail_on_skip: bool,
    vault_name: &str,
    incremental: bool,
    message: &str,
    skipped: &[RefreshSkipRow],
    frontmatter_unparsed: &[RefreshSkipRow],
) -> anyhow::Result<(i32, Option<String>)> {
    let unrequested = skipped
        .iter()
        .filter(|row| !row.excluded_by_request)
        .count();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "vault": vault_name,
                "mode": if incremental { "since" } else { "full" },
                "coverage_status": if unrequested > 0 { "degraded" } else { "complete" },
                "skipped_count": skipped.len(),
                "excluded_by_request_count": skipped.len() - unrequested,
                "skipped_files": skipped.iter().map(|row| row.to_json(true)).collect::<Vec<_>>(),
                "frontmatter_unparsed": frontmatter_unparsed
                    .iter()
                    .map(|row| row.to_json(false))
                    .collect::<Vec<_>>(),
                "message": message,
            }))?
        );
    }
    Ok((
        if fail_on_skip && unrequested > 0 {
            EXIT_ERROR
        } else {
            EXIT_SUCCESS
        },
        None,
    ))
}

/// nw-694: `brain status` text says a vault is BLOCKED. The count lived only
/// in `--json` (`vault_derivation`), so a human saw a healthy status while
/// every link-graph tool refused that vault.
///
/// A derivation the daemon is running now is said too, with its vault, phase
/// and how long it has run, so status explains why link-graph tools wait.
pub(crate) fn vault_derivation_status_lines(status: &serde_json::Value) -> Vec<String> {
    let derivation = status.get("vault_derivation");
    let mut lines = Vec::new();
    if let Some(running) = derivation
        .and_then(|derivation| derivation.get("in_progress"))
        .filter(|running| running.is_object())
    {
        let text = |key: &str| running.get(key).and_then(|v| v.as_str()).unwrap_or("?");
        let mut detail = format!("phase: {}", text("phase"));
        if let Some(notes) = running.get("indexed_notes").and_then(|v| v.as_u64()) {
            detail.push_str(&format!(", {notes} note(s) indexed"));
        }
        if let Some(elapsed) = running.get("elapsed_seconds").and_then(|v| v.as_u64()) {
            detail.push_str(&format!(", running {elapsed}s"));
        }
        lines.push(format!(
            "Vault derivation in progress: {} ({detail}) -- link-graph tools refuse this vault \
             until it finishes",
            text("root_path")
        ));
    }
    let Some(blocked) = derivation
        .and_then(|derivation| derivation.get("blocked_vaults"))
        .and_then(|blocked| blocked.as_array())
    else {
        return lines;
    };
    lines.extend(blocked.iter().map(|vault| {
        let root = vault
            .get("root_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown vault>");
        let reason = vault
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        format!(
            "Vault BLOCKED: {root} ({reason}) -- Markdown link derivation is not current, so \
                 link-graph tools refuse it. Fix the unreadable path and run a full \
                 `nestweaver brain refresh {root}`."
        )
    }));
    lines
}

#[cfg(test)]
mod vault_derivation_status_tests {
    use super::vault_derivation_status_lines;

    /// nw-694: a Blocked vault is said in the text render.
    #[test]
    fn a_blocked_vault_gets_a_status_line_naming_it() {
        let status = serde_json::json!({
            "vault_derivation": {
                "pending_or_blocked_vaults": 1,
                "blocked_vaults": [
                    { "root_path": "/v/brain", "reason": "publication_incomplete" }
                ]
            }
        });
        let lines = vault_derivation_status_lines(&status);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Vault BLOCKED: /v/brain"), "{}", lines[0]);
        assert!(lines[0].contains("brain refresh /v/brain"), "{}", lines[0]);
    }

    /// A derivation running now is said with its vault, phase and progress.
    #[test]
    fn a_running_derivation_gets_a_status_line_naming_it() {
        let status = serde_json::json!({
            "vault_derivation": {
                "pending_or_blocked_vaults": 1,
                "blocked_vaults": [],
                "in_progress": {
                    "vault_uid": "vlt:default:abc",
                    "root_path": "/v/brain",
                    "phase": "refreshing notes",
                    "indexed_notes": 4000,
                    "elapsed_seconds": 12
                }
            }
        });
        let lines = vault_derivation_status_lines(&status);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("Vault derivation in progress: /v/brain"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("phase: refreshing notes"), "{}", lines[0]);
        assert!(lines[0].contains("4000 note(s)"), "{}", lines[0]);
        assert!(lines[0].contains("running 12s"), "{}", lines[0]);
    }

    /// Counterweight: no Blocked vault, no line.
    #[test]
    fn no_blocked_vault_means_no_line() {
        let status = serde_json::json!({
            "vault_derivation": {
                "pending_or_blocked_vaults": 0,
                "blocked_vaults": [],
                "in_progress": null
            }
        });
        assert!(vault_derivation_status_lines(&status).is_empty());
        assert!(vault_derivation_status_lines(&serde_json::json!({})).is_empty());
    }
}

/// nw-511. `brain context`'s not-found report, shared by the daemon and
/// direct routes. The engine's message is the useful part: it says the
/// command resolves a NAME and gives the `nestweaver investigate '<query>'`
/// command to run for a natural-language question. Both `--json` envelopes
/// used to drop it, and the daemon's text route printed the RPC wrapper's
/// top line. Now both routes report the ROOT cause, in `message` on stdout
/// under `--json` (the same key `context` uses) and as the text on stderr.
pub(crate) fn report_brain_context_not_found(
    error: &anyhow::Error,
    json: bool,
    seeds: &[String],
) -> anyhow::Result<i32> {
    let message = brain_context_not_found_message(error);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&brain_context_not_found_payload(&message, seeds))?
        );
    } else {
        eprintln!("{message}");
    }
    Ok(EXIT_NOT_FOUND)
}

/// The engine's "No seeds resolved." sentence from anywhere in the chain,
/// without the transport's wrapper around it.
pub(crate) fn brain_context_not_found_message(error: &anyhow::Error) -> String {
    let rendered: Vec<String> = error.chain().map(ToString::to_string).collect();
    rendered
        .iter()
        .find_map(|cause| {
            cause
                .find("No seeds resolved.")
                .map(|start| cause[start..].to_string())
        })
        .unwrap_or_else(|| format!("{error:#}"))
}

pub(crate) fn brain_context_not_found_payload(
    message: &str,
    seeds: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "error": "not found",
        "status": "not_found",
        "message": message,
        "seeds_expanded": 0,
        "connected": [],
        "unresolved_seeds": seeds,
    })
}

#[cfg(test)]
mod brain_context_not_found_tests {
    use super::*;

    const ENGINE: &str = "No seeds resolved. Tried as UIDs, note titles, tags (with or without '#'), symbol names. This command resolves a NAME (UID, note title, tag, or symbol name) — for a natural-language question, run `nestweaver investigate 'how does auth work'` instead, which falls back to full-text search. Unresolved: [\"how does auth work\"]";

    /// nw-511: the `--json` envelope carries the engine's investigate hint,
    /// and a daemon wrapper around it is stripped (the text route used to
    /// print only the wrapper's top line).
    #[test]
    fn the_not_found_envelope_carries_the_investigate_hint() {
        let seeds = vec!["how does auth work".to_string()];
        let wrapped = anyhow::anyhow!("tool brain_context failed: {ENGINE}")
            .context("brain_context RPC failed");
        let message = brain_context_not_found_message(&wrapped);
        assert_eq!(message, ENGINE);
        let payload = brain_context_not_found_payload(&message, &seeds);
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("nestweaver investigate 'how does auth work'"),
            "{payload}"
        );
        assert_eq!(payload["status"], "not_found");
        assert_eq!(payload["error"], "not found");
        assert_eq!(payload["unresolved_seeds"], serde_json::json!(seeds));
    }

    /// COUNTERWEIGHT: the direct route's bare engine error is used as is.
    #[test]
    fn a_bare_engine_error_is_reported_verbatim() {
        assert_eq!(
            brain_context_not_found_message(&anyhow::anyhow!(ENGINE)),
            ENGINE
        );
    }
}

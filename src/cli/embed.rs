//! The `embed` command.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

/// Pure rendering for the daemon `embed` route's terminal outcome (nw-484):
/// the lines to print and the exit code, with no I/O of its own, so tests
/// can assert on exact strings instead of capturing stderr through a live
/// daemon (which the design explicitly calls out as untestable here — an
/// e2e run against a real Hugging Face endpoint is out of scope for a gate).
///
/// Mirrors `run_embed_with_cancel`'s existing
/// `restart_required`/`rejected`/`stats` branches exactly; the only new
/// behavior is the `model_seeded` line. `repair_identity` +
/// `!resp.identity_repaired` is still the CALLER's job to check first (it is
/// an error, not a line+exit-code pair) — this function assumes that check
/// already passed.
pub(crate) fn render_daemon_embed_outcome(
    resp: &nestweaver_proto::EmbedResponse,
    elapsed: std::time::Duration,
    stats: bool,
) -> (Vec<String>, i32) {
    let mut lines = Vec::new();
    if resp.identity_repaired {
        lines.push(format!(
            "Discarded {} embedding(s) and removed the unreadable semantic identity.",
            resp.discarded_embeddings
        ));
    }
    if resp.restart_required {
        lines.push(
            "Restart the daemon to load the configured embedding model, then run \
             `nestweaver embed --force`."
                .to_string(),
        );
        return (lines, EXIT_SUCCESS);
    }
    if resp.rejected > 0 {
        lines.push(format!(
            "Error: {} embedding(s) rejected by the embedding guards \
             (model or dimension mismatch). Use --force to switch models \
             (clears existing embeddings).",
            resp.rejected
        ));
    }
    // nw-484: this call itself downloaded a missing local model cache and
    // loaded it — no restart. Named once, before the pass summary, because
    // nothing else in this function's output says a download happened.
    // Neither the stale "missing … run `nestweaver embed`" remediation nor
    // any restart advice may appear here: the daemon's own error text is
    // already free of both on every seeding path (server.rs's `embed` RPC),
    // and this function must not reintroduce either on the success path.
    if resp.model_seeded {
        let device = if resp.loaded_device.is_empty() {
            "unknown"
        } else {
            &resp.loaded_device
        };
        lines.push(format!(
            "Downloaded missing embedding model '{}' into {} and loaded it (device: {device}).",
            resp.seeded_model_id, resp.seeded_cache_dir
        ));
    }
    if stats {
        lines.push(format!(
            "Embed stats: {} succeeded, {} failed, \
             {} rejected (model/dim mismatch), {} eligible, \
             {} already embedded, {} scoped node(s), {:.2}s elapsed",
            resp.succeeded,
            resp.failed,
            resp.rejected,
            resp.eligible,
            resp.skipped,
            resp.scoped,
            elapsed.as_secs_f64()
        ));
    } else {
        lines.push(format!(
            "Done: {} embedding(s) generated, {} error(s); \
             {} already embedded out of {} scoped node(s).",
            resp.succeeded, resp.failed, resp.skipped, resp.scoped
        ));
    }
    let exit = if resp.failed > 0 || resp.rejected > 0 {
        EXIT_ERROR
    } else {
        EXIT_SUCCESS
    };
    (lines, exit)
}

/// Generate embeddings for symbols, notes, and/or headings.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_embed<Load>(
    db: Option<&Path>,
    local: bool,
    endpoint: Option<&str>,
    model: Option<&str>,
    model_id: Option<&str>,
    cache_dir: Option<&Path>,
    accelerator: Option<CliEmbeddingAccelerator>,
    batch_size: usize,
    scope: &str,
    force: bool,
    repair_identity: bool,
    stats: bool,
    use_daemon: bool,
    local_model_loader: Load,
) -> anyhow::Result<i32>
where
    Load: Fn(
        &str,
        Option<&Path>,
        Option<CliEmbeddingAccelerator>,
    ) -> anyhow::Result<Box<dyn CliBatchEmbedder>>,
{
    run_embed_with_cancel(
        db,
        local,
        endpoint,
        model,
        model_id,
        cache_dir,
        accelerator,
        batch_size,
        scope,
        force,
        repair_identity,
        stats,
        use_daemon,
        local_model_loader,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_embed_with_cancel<Load>(
    db: Option<&Path>,
    local: bool,
    endpoint: Option<&str>,
    model: Option<&str>,
    model_id: Option<&str>,
    cache_dir: Option<&Path>,
    accelerator: Option<CliEmbeddingAccelerator>,
    batch_size: usize,
    scope: &str,
    force: bool,
    repair_identity: bool,
    stats: bool,
    use_daemon: bool,
    local_model_loader: Load,
    cancel_requested: Option<&dyn Fn() -> anyhow::Result<bool>>,
) -> anyhow::Result<i32>
where
    Load: Fn(
        &str,
        Option<&Path>,
        Option<CliEmbeddingAccelerator>,
    ) -> anyhow::Result<Box<dyn CliBatchEmbedder>>,
{
    let check_cancel = || -> anyhow::Result<()> {
        if let Some(check) = cancel_requested
            && check()?
        {
            anyhow::bail!("embedding cancelled at a safe batch boundary");
        }
        Ok(())
    };
    check_cancel()?;
    // Validate flags
    if local && endpoint.is_some() {
        anyhow::bail!("--local and --endpoint are mutually exclusive");
    }
    if accelerator.is_some() && !local {
        anyhow::bail!("--accelerator requires --local");
    }
    if cache_dir.is_some() && !local {
        anyhow::bail!("--cache-dir requires --local");
    }
    if batch_size == 0 {
        anyhow::bail!("--batch-size must be at least 1");
    }
    let do_symbols = scope == "all" || scope == "symbols";
    let do_notes = scope == "all" || scope == "notes";
    let do_headings = scope == "all" || scope == "headings";
    if !do_symbols && !do_notes && !do_headings {
        anyhow::bail!("unknown --scope '{scope}': expected one of: all, symbols, notes, headings");
    }

    let t0 = std::time::Instant::now();
    let default = default_db_path();
    let path = db.unwrap_or(&default);
    let selected = selected_db_path(path)?;
    require_openable_db(&selected)?;
    let local_model_id = local_embedding_model_id(model_id);

    // ── Daemon path (configured embedding backend) ─────────────────────────
    // Only use daemon for local-model embedding (no --endpoint, no --local) AND
    // when the daemon is enabled. Under --no-daemon / NESTWEAVER_NO_DAEMON=1 we must
    // NOT touch the daemon: connecting auto-starts one, whose held DB lock then
    // breaks the direct path with a confusing "could not set lock" error (and
    // leaks the daemon). Skip straight to the in-process path instead.
    if use_daemon && endpoint.is_none() && !local {
        // The owning daemon validates semantic identity and selects its backend.
        // Client-side metadata reads would open a second database runtime.
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        match rt.block_on(nestweaver_client::DaemonClient::connect(path, None)) {
            Ok(mut client) => {
                if model_id.is_some() {
                    let status = rt
                        .block_on(client.brain_status())
                        .context("read the daemon's selected embedding model")?;
                    let selected = status.embedding_status.as_ref()
                        .map(|embedding| embedding.model_id.as_str())
                        .filter(|model| !model.is_empty())
                        .ok_or_else(|| anyhow::anyhow!(
                            "daemon did not report its selected embedding model; restart it before using --model-id"
                        ))?;
                    daemon_route_model_override_is_honored(model_id, Some(selected))
                        .map_err(anyhow::Error::msg)?;
                }

                if !repair_identity {
                    match rt.block_on(client.plan_embed(scope, force)) {
                        Ok(plan) => {
                            eprintln!(
                                "Embedding plan: {} eligible, {} already embedded, {} scoped node(s).",
                                plan.eligible, plan.skipped, plan.scoped
                            );
                            if plan.eligible == 0 {
                                eprintln!(
                                    "No embedding work required; every scoped node already has an authoritative sidecar embedding."
                                );
                                return Ok(EXIT_SUCCESS);
                            }
                        }
                        Err(error) => {
                            // A same-version daemon from before PlanEmbed can survive a binary
                            // upgrade. Keep the legacy route usable, but make the lost preflight
                            // visible and tell the operator how to enable it.
                            if daemon_lacks_embedding_preflight(&error) {
                                eprintln!(
                                    "Daemon does not support embedding preflight; restart it with \
                                 `nestweaver daemon --db {} restart` to enable eligibility reporting.",
                                    path.display()
                                );
                            } else {
                                return Err(error).context("daemon embedding preflight failed");
                            }
                        }
                    }
                }

                eprintln!("Embedding via daemon (configured backend)…");
                // A3: the daemon route is a single unary RPC, so unlike the
                // direct routes it printed nothing between "Embedding via
                // daemon…" and "Done" — 12h32m of silence in the incident.
                // Poll `brain status` (a read; it does not contend for the
                // write lock) and echo the counters the daemon now publishes.
                // Deliberately polling rather than adding a streaming RPC.
                match rt.block_on(async {
                    let _progress = nestweaver_client::progress::StatusNotifier::spawn(
                        path,
                        "nestweaver embed",
                        nestweaver_client::progress::NoticeKind::EmbedProgress,
                    );
                    client
                        .embed_with_identity_repair(
                            scope,
                            force,
                            batch_size as u32,
                            repair_identity,
                        )
                        .await
                }) {
                    Ok(resp) => {
                        let elapsed = t0.elapsed();
                        if repair_identity && !resp.identity_repaired {
                            anyhow::bail!(
                                "daemon returned without confirming embedding identity repair; \
                                 restart it and retry --repair-identity"
                            );
                        }
                        let (lines, exit_code) = render_daemon_embed_outcome(&resp, elapsed, stats);
                        for line in lines {
                            eprintln!("{line}");
                        }
                        return Ok(exit_code);
                    }
                    Err(e) => {
                        return Err(e).context("daemon embed failed");
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to connect to daemon for {}; start it with \
                         'nestweaver daemon --db {} start'; direct fallback is refused",
                        path.display(),
                        path.display()
                    )
                });
            }
        }
    }

    // ── Direct DB access ──────────────────────────────────────────
    // Only allowed when the daemon path was not selected (--local or --endpoint)
    // or when the daemon is explicitly disabled (--no-daemon / NESTWEAVER_NO_DAEMON=1).
    if use_daemon && endpoint.is_none() && !local {
        anyhow::bail!(
            "daemon is not running. Start it with 'nestweaver daemon --db {} start'; direct fallback is refused",
            path.display()
        );
    }

    // The LOCK, not a pidfile read. This was
    // `if daemon_process_running_for_db(path)` — a point-in-time check of a
    // different file, followed by a write that can run for hours. CWE-367
    // exactly, and MITRE's mitigation leads with the fix: "ensure that locking
    // occurs before the check, as opposed to afterwards."
    //
    // The window here is not theoretical. Every `DaemonClient::connect`
    // autostarts a daemon, so an ordinary `nestweaver search` in another
    // terminal mid-embed hands the store to a second writer while this one is
    // still rewriting `.embeddings.bin`. A pidfile also cannot survive PID
    // reuse, and an operator's `rm` erases it; the database lock has neither
    // hole.
    // Held for the whole pass, which can run for hours.
    // The direct pass embeds what the daemon would serve, not a base database
    // a publication rebuild left behind.
    let path = selected.as_path();
    let write_lease = require_exclusive_store_access(path, "embed")?;

    let store =
        nestweaver_store::GraphStore::open_with_authority(path, &write_lease).map_err(|e| {
            anyhow::anyhow!(
                "failed to open database for writing at {}: {e}",
                path.display()
            )
        })?;
    if repair_identity {
        let discarded = store
            .reset_embedding_space_for_identity_repair()
            .context("discard the unreadable semantic space before direct embedding")?;
        eprintln!(
            "Discarded {discarded} embedding(s) and removed the unreadable semantic identity; rebuilding from the requested model."
        );
    }
    // Validate the complete persisted pipeline before model loading, network
    // calls, or local inference. `--force` may replace a verified identity; it
    // must never authorize spending work against an unreadable one.
    store
        .require_verified_embedding_identity()
        .context("verify the database embedding identity before direct embedding")?;
    store.reset_embedding_force_guard();
    let force = force || repair_identity;

    // The direct paths (--local / --endpoint) resolve their model from flags
    // alone and bypass the daemon route's recorded-model guard at the top of
    // this function. Apply the same check here — reusing
    // daemon_route_model_override_is_honored so the routes cannot drift — so a
    // conflicting explicit model requires --force. Absent metadata means the
    // database was never stamped: unknown, so proceed (first embed).
    let recorded_model_id = store
        .get_embedding_pipeline()
        .context("read the complete database embedding pipeline before direct embedding")?
        .map(|pipeline| pipeline.model_id);
    if !force && let Some(recorded) = recorded_model_id.as_deref() {
        // On the endpoint branch the comparison operand is the endpoint's
        // model (--model, defaulting to the external default), never
        // --model-id — comparing --model-id would bail on every correctly
        // configured external run.
        let requested = if endpoint.is_some() {
            Some(external_embedding_model(model))
        } else {
            model_id
        };
        if daemon_route_model_override_is_honored(requested, Some(recorded)).is_err() {
            // In the Err case `requested` is always Some (None is Ok).
            anyhow::bail!(
                "embedding model mismatch: the database was embedded with '{recorded}' but this \
                 run requested '{}'; use the recorded model or pass --force to switch models \
                 (re-embeds everything)",
                requested.unwrap_or_default()
            );
        }
    }

    let mut success_count = 0usize;
    let mut error_count = 0usize;
    let mut rejected_count = 0usize;
    // Dimension of the vectors THIS run produced, set on the first accepted
    // write. The metadata stamp below is gated on it: a run that produced
    // nothing (everything already embedded, or every batch rejected) must not
    // overwrite the recorded fingerprint with a fabricated (model, dimension)
    // pair taken from pre-existing vectors.
    let mut produced_dim: Option<usize> = None;
    let mut produced_pipeline: Option<nestweaver_schema::EmbeddingPipelineV2> = None;
    // Checkpoint the index to its bounded append journal about every five
    // minutes so an interrupted pass keeps completed work without rewriting
    // the mmap base after every batch.
    let mut flush_checkpoint = nestweaver_store::EmbeddingFlushCheckpoint::new(
        nestweaver_store::EMBED_CHECKPOINT_INTERVAL,
    );

    if let Some(ep) = endpoint {
        // ── External API path ────────────────────────────────────
        let api_model = external_embedding_model(model);
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

        if do_symbols {
            let all = store
                .list_all_symbols()
                .map_err(|e| anyhow::anyhow!(e))
                .context("list_all_symbols")?;
            let to_embed: Vec<_> = if force {
                all.iter().collect()
            } else {
                all.iter()
                    .filter(|s| !store.has_embedding(&s.uid))
                    .collect()
            };
            let total = to_embed.len();
            if total > 0 {
                eprintln!("Embedding {total} symbol(s) via API (batch size {batch_size})…");
                for (batch_idx, chunk) in to_embed.chunks(batch_size).enumerate() {
                    check_cancel()?;
                    let done = batch_idx * batch_size + chunk.len();
                    eprint!("\rEmbedding symbols... {done}/{total}");
                    let texts: Vec<String> = chunk
                        .iter()
                        .map(|sym| {
                            if sym.signature.is_empty() {
                                sym.name.clone()
                            } else {
                                sym.signature.clone()
                            }
                        })
                        .collect();
                    let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                    match rt.block_on(generate_embeddings_batch(ep, api_model, &text_refs)) {
                        Ok(embeddings) => {
                            for (sym, emb) in chunk.iter().zip(embeddings) {
                                let emb_dim = emb.len();
                                let pipeline = nestweaver_schema::EmbeddingPipelineV2::external(
                                    "openai-compatible",
                                    api_model,
                                    u32::try_from(emb_dim)?,
                                );
                                if store
                                    .add_embedding_with_pipeline(&sym.uid, emb, &pipeline, force)
                                {
                                    success_count += 1;
                                    produced_pipeline.get_or_insert(pipeline);
                                    if produced_dim.is_none() {
                                        produced_dim = Some(emb_dim);
                                    }
                                } else {
                                    rejected_count += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("\n    Warning: batch embedding API error: {e}");
                            error_count += chunk.len();
                        }
                    }
                    if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                        &store,
                        success_count,
                        produced_pipeline.as_ref(),
                    ) {
                        eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                    }
                }
                eprintln!();
            }
        }

        if do_notes {
            let all = store
                .list_notes(None)
                .map_err(|e| anyhow::anyhow!(e))
                .context("list_notes")?;
            let to_embed: Vec<_> = if force {
                all.iter().collect()
            } else {
                all.iter()
                    .filter(|n| !store.has_embedding(&n.uid))
                    .collect()
            };
            let total = to_embed.len();
            if total > 0 {
                eprintln!("Embedding {total} note(s) via API (batch size {batch_size})…");
                for (batch_idx, chunk) in to_embed.chunks(batch_size).enumerate() {
                    check_cancel()?;
                    let done = batch_idx * batch_size + chunk.len();
                    eprint!("\rEmbedding notes... {done}/{total}");
                    let texts: Vec<String> = chunk.iter().map(|n| n.title.clone()).collect();
                    let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                    match rt.block_on(generate_embeddings_batch(ep, api_model, &text_refs)) {
                        Ok(embeddings) => {
                            for (note, emb) in chunk.iter().zip(embeddings) {
                                let emb_dim = emb.len();
                                let pipeline = nestweaver_schema::EmbeddingPipelineV2::external(
                                    "openai-compatible",
                                    api_model,
                                    u32::try_from(emb_dim)?,
                                );
                                if store
                                    .add_embedding_with_pipeline(&note.uid, emb, &pipeline, force)
                                {
                                    success_count += 1;
                                    produced_pipeline.get_or_insert(pipeline);
                                    if produced_dim.is_none() {
                                        produced_dim = Some(emb_dim);
                                    }
                                } else {
                                    rejected_count += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("\n    Warning: batch embedding API error: {e}");
                            error_count += chunk.len();
                        }
                    }
                    if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                        &store,
                        success_count,
                        produced_pipeline.as_ref(),
                    ) {
                        eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                    }
                }
                eprintln!();
            }
        }

        if do_headings {
            let all_headings = store
                .list_all_headings()
                .map_err(|e| anyhow::anyhow!(e))
                .context("list_all_headings")?;
            let to_embed: Vec<_> = if force {
                all_headings.iter().collect()
            } else {
                all_headings
                    .iter()
                    .filter(|h| !store.has_embedding(&h.uid))
                    .collect()
            };
            let total = to_embed.len();
            if total > 0 {
                // Build note title lookup
                let notes = store.list_notes(None).map_err(|e| anyhow::anyhow!(e))?;
                let note_titles: std::collections::HashMap<&str, &str> = notes
                    .iter()
                    .map(|n| (n.uid.as_str(), n.title.as_str()))
                    .collect();

                eprintln!("Embedding {total} heading(s) via API (batch size {batch_size})…");
                for (batch_idx, chunk) in to_embed.chunks(batch_size).enumerate() {
                    check_cancel()?;
                    let done = batch_idx * batch_size + chunk.len();
                    eprint!("\rEmbedding headings... {done}/{total}");
                    let texts: Vec<String> = chunk
                        .iter()
                        .map(|h| {
                            let note_title =
                                note_titles.get(h.note_uid.as_str()).copied().unwrap_or("");
                            if note_title.is_empty() {
                                h.text.clone()
                            } else {
                                format!("{note_title} > {}", h.text)
                            }
                        })
                        .collect();
                    let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                    match rt.block_on(generate_embeddings_batch(ep, api_model, &text_refs)) {
                        Ok(embeddings) => {
                            for (h, emb) in chunk.iter().zip(embeddings) {
                                let emb_dim = emb.len();
                                let pipeline = nestweaver_schema::EmbeddingPipelineV2::external(
                                    "openai-compatible",
                                    api_model,
                                    u32::try_from(emb_dim)?,
                                );
                                if !force
                                    && let Some(diagnostic) =
                                        store.embedding_pipeline_mismatch(&pipeline)?
                                {
                                    eprintln!("embedding pipeline mismatch: {diagnostic}");
                                    return Ok(EXIT_ERROR);
                                }
                                if store.add_embedding_with_pipeline(&h.uid, emb, &pipeline, force)
                                {
                                    success_count += 1;
                                    produced_pipeline.get_or_insert(pipeline);
                                    if produced_dim.is_none() {
                                        produced_dim = Some(emb_dim);
                                    }
                                } else {
                                    rejected_count += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("\n    Warning: batch embedding API error: {e}");
                            error_count += chunk.len();
                        }
                    }
                    if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                        &store,
                        success_count,
                        produced_pipeline.as_ref(),
                    ) {
                        eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                    }
                }
                eprintln!();
            }
        }
    } else {
        // ── Local model path (default) ───────────────────────────
        #[cfg(feature = "embed")]
        {
            let embed_model = local_model_loader(local_model_id, cache_dir, accelerator)
                .context("failed to load local embedding model")?;

            if do_symbols {
                let all = store
                    .list_all_symbols()
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("list_all_symbols")?;
                let to_embed: Vec<_> = if force {
                    all.iter().collect()
                } else {
                    all.iter()
                        .filter(|s| !store.has_embedding(&s.uid))
                        .collect()
                };
                let total = to_embed.len();
                if total > 0 {
                    eprintln!("Embedding {total} symbol(s) with local model…");
                    for (batch_idx, batch) in to_embed.chunks(batch_size).enumerate() {
                        check_cancel()?;
                        let done = batch_idx * batch_size + batch.len();
                        eprint!("\rEmbedding symbols... {done}/{total}");
                        let texts: Vec<String> = batch
                            .iter()
                            .map(|s| {
                                nestweaver_embed::preprocess::symbol_embed_text(
                                    &s.kind.to_string(),
                                    &s.name,
                                    None,
                                )
                            })
                            .collect();
                        let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                        match embed_model.embed_batch(&text_refs) {
                            Ok(embeddings) => {
                                for (sym, emb) in batch.iter().zip(embeddings.iter()) {
                                    let pipeline = embed_model
                                        .pipeline_for_dimension(local_model_id, emb.len())?;
                                    if !force
                                        && let Some(diagnostic) =
                                            store.embedding_pipeline_mismatch(&pipeline)?
                                    {
                                        eprintln!("embedding pipeline mismatch: {diagnostic}");
                                        return Ok(EXIT_ERROR);
                                    }
                                    if store.add_embedding_with_pipeline(
                                        &sym.uid,
                                        emb.clone(),
                                        &pipeline,
                                        force,
                                    ) {
                                        success_count += 1;
                                        produced_pipeline.get_or_insert(pipeline);
                                        if produced_dim.is_none() {
                                            produced_dim = Some(emb.len());
                                        }
                                    } else {
                                        rejected_count += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("\n    Warning: local embed error: {e}");
                                error_count += batch.len();
                            }
                        }
                        if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                            &store,
                            success_count,
                            produced_pipeline.as_ref(),
                        ) {
                            eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                        }
                    }
                    eprintln!();
                }
            }

            if do_notes {
                let all = store
                    .list_notes(None)
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("list_notes")?;
                let to_embed: Vec<_> = if force {
                    all.iter().collect()
                } else {
                    all.iter()
                        .filter(|n| !store.has_embedding(&n.uid))
                        .collect()
                };
                let total = to_embed.len();
                if total > 0 {
                    eprintln!("Embedding {total} note(s) with local model…");
                    for (batch_idx, batch) in to_embed.chunks(batch_size).enumerate() {
                        check_cancel()?;
                        let done = batch_idx * batch_size + batch.len();
                        eprint!("\rEmbedding notes... {done}/{total}");
                        let texts: Vec<String> = batch
                            .iter()
                            .map(|n| nestweaver_embed::preprocess::note_embed_text(&n.title, None))
                            .collect();
                        let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                        match embed_model.embed_batch(&text_refs) {
                            Ok(embeddings) => {
                                for (note, emb) in batch.iter().zip(embeddings.iter()) {
                                    let pipeline = embed_model
                                        .pipeline_for_dimension(local_model_id, emb.len())?;
                                    if !force
                                        && let Some(diagnostic) =
                                            store.embedding_pipeline_mismatch(&pipeline)?
                                    {
                                        eprintln!("embedding pipeline mismatch: {diagnostic}");
                                        return Ok(EXIT_ERROR);
                                    }
                                    if store.add_embedding_with_pipeline(
                                        &note.uid,
                                        emb.clone(),
                                        &pipeline,
                                        force,
                                    ) {
                                        success_count += 1;
                                        produced_pipeline.get_or_insert(pipeline);
                                        if produced_dim.is_none() {
                                            produced_dim = Some(emb.len());
                                        }
                                    } else {
                                        rejected_count += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("\n    Warning: local embed error: {e}");
                                error_count += batch.len();
                            }
                        }
                        if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                            &store,
                            success_count,
                            produced_pipeline.as_ref(),
                        ) {
                            eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                        }
                    }
                    eprintln!();
                }
            }

            if do_headings {
                let all_headings = store
                    .list_all_headings()
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("list_all_headings")?;
                let to_embed: Vec<_> = if force {
                    all_headings.iter().collect()
                } else {
                    all_headings
                        .iter()
                        .filter(|h| !store.has_embedding(&h.uid))
                        .collect()
                };
                let total = to_embed.len();
                if total > 0 {
                    let notes = store.list_notes(None).map_err(|e| anyhow::anyhow!(e))?;
                    let note_titles: std::collections::HashMap<&str, &str> = notes
                        .iter()
                        .map(|n| (n.uid.as_str(), n.title.as_str()))
                        .collect();

                    eprintln!("Embedding {total} heading(s) with local model…");
                    for (batch_idx, batch) in to_embed.chunks(batch_size).enumerate() {
                        check_cancel()?;
                        let done = batch_idx * batch_size + batch.len();
                        eprint!("\rEmbedding headings... {done}/{total}");
                        let texts: Vec<String> = batch
                            .iter()
                            .map(|h| {
                                let note_title =
                                    note_titles.get(h.note_uid.as_str()).copied().unwrap_or("");
                                nestweaver_embed::preprocess::heading_embed_text(
                                    note_title, &h.text,
                                )
                            })
                            .collect();
                        let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
                        match embed_model.embed_batch(&text_refs) {
                            Ok(embeddings) => {
                                for (h, emb) in batch.iter().zip(embeddings.iter()) {
                                    let pipeline = embed_model
                                        .pipeline_for_dimension(local_model_id, emb.len())?;
                                    if !force
                                        && let Some(diagnostic) =
                                            store.embedding_pipeline_mismatch(&pipeline)?
                                    {
                                        eprintln!("embedding pipeline mismatch: {diagnostic}");
                                        return Ok(EXIT_ERROR);
                                    }
                                    if store.add_embedding_with_pipeline(
                                        &h.uid,
                                        emb.clone(),
                                        &pipeline,
                                        force,
                                    ) {
                                        success_count += 1;
                                        produced_pipeline.get_or_insert(pipeline);
                                        if produced_dim.is_none() {
                                            produced_dim = Some(emb.len());
                                        }
                                    } else {
                                        rejected_count += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("\n    Warning: local embed error: {e}");
                                error_count += batch.len();
                            }
                        }
                        if let Err(e) = flush_checkpoint.flush_if_due_with_pipeline(
                            &store,
                            success_count,
                            produced_pipeline.as_ref(),
                        ) {
                            eprintln!("\n    Warning: failed to checkpoint embedding index: {e}");
                        }
                    }
                    eprintln!();
                }
            }
        }

        #[cfg(not(feature = "embed"))]
        {
            let _ = &local_model_loader;
            anyhow::bail!(
                "local embedding requires the `embed` feature; \
                 rebuild with `--features embed` or pass --endpoint"
            );
        }
    }

    if rejected_count > 0 {
        eprintln!(
            "Error: {rejected_count} embedding(s) rejected by the embedding guards \
             (model or dimension mismatch). Use --force to switch models \
             (clears existing embeddings)."
        );
    }

    // Record which embedding model produced these vectors, so the daemon loads a matching
    // model at startup regardless of the compiled default or the instance config (see
    // run_server). This is what lets the shipped default stay light for most users while a
    // given DB transparently uses whatever model it was embedded with.
    //
    // The stamped pair must describe vectors THIS run produced: the dimension
    // comes from `produced_dim` (set on the first accepted write), never from
    // pre-existing vectors, and a run that produced nothing — everything
    // already embedded, or every batch rejected — stamps nothing. The stamp
    // also sits below the rejection warning so a fully-rejected run cannot
    // write the pair before the warning says the writes failed.
    if let Some(pipeline) = produced_pipeline.as_ref() {
        match store.set_embedding_pipeline(pipeline) {
            Ok(()) => {
                if let Err(e) = store.flush_embedding_index() {
                    eprintln!("Warning: failed to save embedding sidecar: {e}");
                    // NOTHING persisted, so nothing succeeded. `error_count +=
                    // success_count` without zeroing reported both — "4821
                    // embedding(s) generated, 4821 error(s)", and with --stats
                    // 9642 outcomes for 4821 items.
                    //
                    // This is the fsyncgate shape: PostgreSQL spent twenty
                    // years reporting durable success for writes the kernel had
                    // discarded, and the remedy was to treat a failed flush as
                    // fatal rather than to keep counting what it thought it had
                    // written. A count of successes must mean "confirmed
                    // persisted".
                    error_count += success_count;
                    success_count = 0;
                }
            }
            Err(e) => {
                // Never write vectors under absent or stale pipeline metadata:
                // open-time trust requires both to describe one exact space.
                eprintln!(
                    "Warning: failed to record embedding pipeline; sidecar was not saved: {e}"
                );
                // Same reasoning: the sidecar was not saved, so the successes
                // did not survive the run.
                error_count += success_count;
                success_count = 0;
            }
        }
    } else if let Some(requested) = if endpoint.is_some() { model } else { model_id }
        && let Some((recorded, _)) = store
            .get_embedding_metadata()
            .context("read the database embedding identity after a zero-work embed")?
        && recorded != requested
    {
        // Zero-work path with an explicit model mismatch: name it, or the
        // run ends with "Done: 0 embedding(s)" and no hint that the
        // requested model was never applied.
        eprintln!(
            "No embeddings were produced; the database remains embedded with '{recorded}'. \
             Re-run with --force to re-embed everything with '{requested}'."
        );
    }

    if stats {
        let elapsed = t0.elapsed();
        eprintln!(
            "Embed stats: {success_count} succeeded, {error_count} failed, \
             {rejected_count} rejected (model/dim mismatch), {:.2}s elapsed",
            elapsed.as_secs_f64()
        );
    } else {
        eprintln!("Done: {success_count} embedding(s) generated, {error_count} error(s).");
    }

    drop(store);

    if error_count > 0 || rejected_count > 0 {
        Ok(EXIT_ERROR)
    } else {
        Ok(EXIT_SUCCESS)
    }
}

pub(crate) fn daemon_lacks_embedding_preflight(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| status.code() == tonic::Code::Unimplemented)
}

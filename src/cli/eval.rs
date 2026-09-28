//! Ranking and retrieval-evaluation commands (`ranking`, `eval`, `rts-eval`).
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

/// Resolve a node UID to its file-path location for ranking-prior matching.
/// Mirrors the location each kind renders with in brain results.
pub(crate) fn ranking_location_for_uid(
    store: &nestweaver_store::GraphStore,
    uid: &str,
) -> Option<String> {
    if uid.starts_with("sym:") {
        let s = store.lookup_symbol(uid).ok()?;
        Some(format!("{}:{}", s.file_path, s.start_line))
    } else if uid.starts_with("note:") {
        store.lookup_note(uid).ok().map(|n| n.file_path)
    } else if uid.starts_with("sec:") {
        let sec = store.lookup_section(uid).ok()?;
        store.lookup_note(&sec.note_uid).ok().map(|n| n.file_path)
    } else if uid.starts_with("head:") {
        let h = store.lookup_heading(uid).ok()?;
        store.lookup_note(&h.note_uid).ok().map(|n| n.file_path)
    } else {
        None
    }
}

/// Build a [`HybridSearchConfig`] for the eval harness with the given PRF
/// toggle, otherwise defaults (identical to the product's default retrieval).
pub(crate) fn eval_hybrid_config(prf: bool) -> HybridSearchConfig {
    HybridSearchConfig {
        prf,
        ..HybridSearchConfig::default()
    }
}

/// Print the per-query table and aggregate for an `EvalReport` (human form).
pub(crate) fn print_eval_report(report: &nestweaver_engine::EvalReport) {
    println!("Query                                              nDCG@10    MRR  P@5");
    println!("{}", "-".repeat(78));
    for row in &report.per_query {
        let q: String = row.query.chars().take(48).collect();
        println!(
            "{q:<48}  {:>7.4}  {:>5.3}  {:>4.2}",
            row.ndcg10, row.mrr, row.p_at_5
        );
    }
    println!("{}", "-".repeat(78));
    println!(
        "MEAN over {} quer{}:  nDCG@10={:.4}  MRR={:.4}  P@5={:.4}",
        report.n,
        if report.n == 1 { "y" } else { "ies" },
        report.mean_ndcg10,
        report.mean_mrr,
        report.mean_p5,
    );
}

/// Dispatch an `eval` subcommand (P0.3 retrieval-quality harness).
pub(crate) fn run_eval_cmd(command: EvalCommands, _use_daemon: bool) -> anyhow::Result<i32> {
    // Honest-framing banner shown on every human-readable run.
    const HONEST_NOTE: &str = "Note: meaningful evaluation requires REAL human relevance labels over your actual\n      corpus. A tiny/synthetic set is NOT authoritative — inspect per-query\n      win/loss and confidence, and use time/query-based splits, before trusting a\n      small mean delta.";

    match command {
        EvalCommands::Run {
            queries,
            db,
            json,
            prf,
            rerank,
        } => {
            let queries_data = nestweaver_engine::load_judged_queries(&queries)?;
            let db_path = resolve_db_with_config(db, None)?;
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            let aliases = load_alias_sidecar(&db_path);

            let cfg = eval_hybrid_config(prf);
            let report = nestweaver_engine::run_eval(
                &store,
                tantivy.as_ref(),
                &queries_data,
                &cfg,
                &aliases,
                Some(&db_path),
                rerank,
            )?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_eval_report(&report);
                println!("\n{HONEST_NOTE}");
            }
            Ok(EXIT_SUCCESS)
        }
        EvalCommands::Compare {
            queries,
            db,
            json,
            prf,
            rerank,
        } => {
            if !prf && !rerank {
                anyhow::bail!("eval compare needs a feature to toggle: pass --prf and/or --rerank");
            }
            let queries_data = nestweaver_engine::load_judged_queries(&queries)?;
            let db_path = resolve_db_with_config(db, None)?;
            let store = open_store(Some(&db_path))?;
            let tantivy_path = tantivy_sidecar_path_for(&db_path);
            let tantivy = TantivyIndex::open_reader_only(&tantivy_path).ok();
            let aliases = load_alias_sidecar(&db_path);

            // Baseline: feature(s) OFF. Treatment: the chosen toggle(s) ON.
            // PRF lives in the HybridSearchConfig; rerank is a run_eval flag.
            let baseline_cfg = eval_hybrid_config(false);
            let treatment_cfg = eval_hybrid_config(prf);

            let run = |cfg: &HybridSearchConfig, do_rerank: bool| {
                nestweaver_engine::run_eval(
                    &store,
                    tantivy.as_ref(),
                    &queries_data,
                    cfg,
                    &aliases,
                    Some(&db_path),
                    do_rerank,
                )
            };
            let baseline = run(&baseline_cfg, false)?;
            let treatment = run(&treatment_cfg, rerank)?;

            let mut toggles = Vec::new();
            if prf {
                toggles.push("prf");
            }
            if rerank {
                toggles.push("rerank");
            }
            let label = toggles.join("+");
            let cmp = nestweaver_engine::compare_reports(
                format!("{label}-off"),
                baseline,
                format!("{label}-on"),
                treatment,
            );

            if json {
                println!("{}", serde_json::to_string_pretty(&cmp)?);
            } else {
                println!(
                    "Comparison: {} (baseline) vs {} (treatment) over {} quer{}",
                    cmp.baseline_label,
                    cmp.treatment_label,
                    cmp.baseline.n,
                    if cmp.baseline.n == 1 { "y" } else { "ies" },
                );
                println!("  baseline  mean nDCG@10 = {:.4}", cmp.baseline.mean_ndcg10);
                println!(
                    "  treatment mean nDCG@10 = {:.4}",
                    cmp.treatment.mean_ndcg10
                );
                println!(
                    "  delta = {:+.4}  ({:+.1}% relative)",
                    cmp.mean_ndcg10_delta,
                    cmp.mean_ndcg10_rel_delta * 100.0,
                );
                println!(
                    "  per-query: {} win(s), {} loss(es), {} tie(s)",
                    cmp.wins, cmp.losses, cmp.ties,
                );
                let gate = cmp.mean_ndcg10_rel_delta >= 0.05;
                println!(
                    "  >= 5% nDCG@10 gate: {}",
                    if gate {
                        "MET (mean only — confirm with per-query win/loss + a larger set)"
                    } else {
                        "NOT met"
                    }
                );
                println!("\n{HONEST_NOTE}");
            }
            Ok(EXIT_SUCCESS)
        }
    }
}

/// Dispatch a `ranking` subcommand.
pub(crate) fn run_ranking(
    command: RankingCommands,
    t0: std::time::Instant,
    _use_daemon: bool,
) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        RankingCommands::Explain {
            uid,
            json,
            db,
            config,
            base_relevance,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;

            let ranking = load_instance_config_opt(config.as_deref())
                .map(|c| c.ranking)
                .unwrap_or_default();

            // Resolve the node's location (the path globs are matched against).
            // Exit 2 when the uid doesn't resolve to a node, consistent with
            // `symbol`/`impact`/`ranking rank`.
            let location = match ranking_location_for_uid(&store, &uid) {
                Some(loc) => loc,
                None => {
                    if json {
                        println!("{}", serde_json::json!({"error": "not found", "uid": uid}));
                    } else {
                        eprintln!("uid '{uid}' not found.");
                    }
                    return Ok((EXIT_NOT_FOUND, None));
                }
            };

            // Delegate the matching + clamping to the engine so the math matches
            // exactly what brain context / search apply.
            let (matched, final_relevance) =
                nestweaver_engine::explain_ranking_prior(&location, base_relevance, &ranking);

            if json {
                let matched_json = match &matched {
                    Some((glob, mult)) => serde_json::json!({
                        "glob": glob,
                        "multiplier": mult,
                    }),
                    None => serde_json::Value::Null,
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uid": uid,
                        "location": location,
                        "base_relevance": base_relevance,
                        "matched_rule": matched_json,
                        "final_relevance": final_relevance,
                    }))?
                );
            } else {
                println!("uid:             {uid}");
                println!("location:        {location}");
                println!("base_relevance:  {base_relevance}");
                match &matched {
                    Some((glob, mult)) => {
                        println!("matched_rule:    {glob} (x{mult})");
                    }
                    None => println!("matched_rule:    none"),
                }
                println!("final_relevance: {final_relevance}");
            }
            Ok((
                EXIT_SUCCESS,
                Some(format!("done in {}", format_elapsed(t0.elapsed()))),
            ))
        }
        RankingCommands::Rank {
            uid,
            json,
            db,
            config,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            let store = open_store(Some(&db_path))?;

            // Apply the configured git-activity weight if a config carries one.
            if let Some(cfg) = load_instance_config_opt(config.as_deref()) {
                store.set_git_activity_weight(cfg.ranking.git_activity_weight);
            }

            // Resolve name-or-uid → uid, then load the symbol.
            let resolved = match resolve_uid(&store, &uid)? {
                ResolveResult::Found(u) => u,
                ResolveResult::NotFound => {
                    eprintln!("Symbol not found: {uid}");
                    return Ok((EXIT_NOT_FOUND, None));
                }
                ResolveResult::Ambiguous(matches) => {
                    eprintln!("Ambiguous symbol '{uid}' — {} matches:", matches.len());
                    for m in matches.iter().take(10) {
                        eprintln!("  {} ({}:{})", m.uid, m.file_path, m.start_line);
                    }
                    return Ok((EXIT_AMBIGUOUS, None));
                }
            };

            let sym = store
                .lookup_symbol(&resolved)
                .map_err(|e| anyhow::anyhow!(e))?;
            let base_pagerank = store
                .pagerank_scores()
                .map_err(|e| anyhow::anyhow!(e))?
                .get(&resolved)
                .copied()
                .unwrap_or(0.0);
            let git_activity_score = store.git_activity_score(&sym.repo_uid, &sym.file_path);
            let weight = store.git_activity_weight();
            let multiplier = nestweaver_store::git_activity_multiplier(git_activity_score, weight);
            let final_rank = base_pagerank * multiplier;

            // nw-370: this command prints a raw `base_pagerank` to eight
            // decimal places and said nothing about where it came from. A
            // number rendered that precisely reads as authoritative, and on a
            // generation-stale graph it is PageRank over the edges an older
            // resolver wrote — the exact quantity nw-103 corrupted. Same
            // disclosure as `hubs`.
            //
            // There is no daemon route to mirror: `ranking rank` opens the
            // store directly and has no `try_hybrid_json_rpc` guard, so the
            // direct constructor is the only one reachable.
            warn_stale_resolver_rankings(&store, &db_path);
            let staleness = ResolverStaleness::from_store(&store, &db_path);

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uid": resolved,
                        "file_path": sym.file_path,
                        "base_pagerank": base_pagerank,
                        "git_activity_score": git_activity_score,
                        "git_activity_weight": weight,
                        "multiplier": multiplier,
                        "final_rank": final_rank,
                        // nw-308's contract: both keys, always. `base_pagerank`
                        // is the whole answer here, so an agent reading it needs
                        // to know whether the edges under it are current.
                        "rankings_stale": staleness.rankings_stale,
                        "stale_repos": staleness.stale_repos,
                    }))?
                );
            } else {
                println!("uid:                {resolved}");
                println!("file_path:          {}", sym.file_path);
                println!("base_pagerank:      {base_pagerank:.8}");
                match git_activity_score {
                    Some(s) => println!("git_activity_score: {s:.4}"),
                    None => println!("git_activity_score: (none → neutral)"),
                }
                println!("multiplier:         {multiplier:.4} (weight {weight})");
                println!("final_rank:         {final_rank:.8}");
            }
            Ok((
                EXIT_SUCCESS,
                Some(format!("done in {}", format_elapsed(t0.elapsed()))),
            ))
        }
    }
}

pub(crate) fn run_rts_eval(command: RtsEvalCommands) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        RtsEvalCommands::RecordTruth {
            sha,
            repo,
            failed_test_files,
            none_failed,
            total_test_files,
            flaky_test_files,
            reruns,
            db,
        } => {
            if failed_test_files.is_empty() && !none_failed {
                anyhow::bail!(
                    "provide --failed-test-files <paths...> or --none-failed for a green run"
                );
            }
            let db_path = db.unwrap_or_else(default_db_path);
            nestweaver_engine::rts_eval::record_truth(
                &db_path,
                repo.as_deref().unwrap_or(""),
                &sha,
                &failed_test_files,
                total_test_files,
                &flaky_test_files,
                reruns,
            )?;
            println!(
                "Recorded full-suite outcome for {} ({} failed test file(s)).",
                sha.chars().take(8).collect::<String>(),
                failed_test_files.len()
            );
            Ok((EXIT_SUCCESS, None))
        }
        RtsEvalCommands::Report { json, window, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            let report = nestweaver_engine::rts_eval::compute_report(&db_path, window)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.insufficient_data {
                println!(
                    "Insufficient data: {} joined selection/truth pair(s) (need {}).",
                    report.n_joined,
                    nestweaver_engine::rts_eval::MIN_JOINED_FOR_METRICS
                );
                println!(
                    "  unresolved selections: {}  unmatched truths: {}",
                    report.n_unresolved_selections, report.n_unmatched_truths
                );
                println!("No recall percentages are reported below that bar — keep feeding");
                println!("full-suite outcomes via `nestweaver rts-eval record-truth`.");
            } else {
                let pct = |v: Option<f64>| {
                    v.map(|x| format!("{:.1}%", x * 100.0))
                        .unwrap_or_else(|| "n/a".to_string())
                };
                println!(
                    "RTS eval over last {} joined pair(s) ({} failing):",
                    report.n_joined, report.n_failing_pairs
                );
                println!("  file recall:        {}", pct(report.file_recall));
                println!("  change recall:      {}", pct(report.change_recall));
                println!("  selection breadth:  {}", pct(report.selection_breadth));
                println!("  time saved (proxy): {}", pct(report.time_saved_proxy));
                println!(
                    "  unresolved selections: {}  unmatched truths: {}",
                    report.n_unresolved_selections, report.n_unmatched_truths
                );
                if report.excluded_flaky_failures > 0 {
                    println!(
                        "  excluded {} failure(s) reported as flaky",
                        report.excluded_flaky_failures
                    );
                }
                if report.recall_estimate_uncertain {
                    println!();
                    println!(
                        "  NOTE: {} run(s) reported failures that were never re-run, so these",
                        report.unconfirmed_failure_runs
                    );
                    println!("  recall figures are UNCERTAIN in either direction (not a bound).");
                    println!("  Pass --reruns (and --flaky) to rts-eval record-truth to report");
                    println!("  confirmed failures.");
                }
            }
            Ok((EXIT_SUCCESS, None))
        }
    }
}

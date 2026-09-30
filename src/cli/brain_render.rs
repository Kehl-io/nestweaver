//! `brain` scope filters, truncation and text/JSON renderers.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

/// The CLI's direct path has no authorization-scope concept, so `visible` is
/// always `None` — a no-op filter over `engine_resolve_repo_filter`, not a
/// second implementation of it. The message is passed through unchanged
/// rather than re-wrapped, so the CLI and MCP report the identical text for
/// an unresolvable `--repos` entry.
pub(crate) fn resolve_repo_filter(
    store: &GraphStore,
    selectors: &[String],
) -> Result<std::collections::HashSet<String>, anyhow::Error> {
    engine_resolve_repo_filter(store, selectors, None)
}

pub(crate) fn resolve_vault_filter(
    store: &GraphStore,
    selectors: &[String],
) -> Result<std::collections::HashSet<String>, anyhow::Error> {
    engine_resolve_vault_filter(store, selectors)
}

/// Keep only nodes that actually carry one of the requested tags.
///
/// nw-407. Extracted from the `--tags` closure so the one decision it encodes
/// is assertable: a Symbol is NOT exempt. See the call site for the measured
/// 30 -> 71 expansion the old unconditional `return true` produced.
pub(crate) fn retain_tagged_nodes(
    nodes: &mut Vec<nestweaver_engine::BrainNode>,
    tagged_notes: &std::collections::HashSet<String>,
    tagged_sections: &std::collections::HashSet<String>,
) {
    nodes.retain(|node| tagged_notes.contains(&node.uid) || tagged_sections.contains(&node.uid));
}

/// Greedy token-budget selection: include nodes in PPR-rank order until the
/// next one would exceed the budget. Returns the count of nodes to take.
/// Token cost per node = (rendered length) / 4 — the standard cheap estimate.
pub(crate) fn token_budgeted_truncate(
    connected: &[nestweaver_engine::BrainNode],
    budget: usize,
    concise: bool,
) -> usize {
    let mut tokens = 0usize;
    let mut taken = 0usize;
    for n in connected {
        let cost = render_cost_tokens(n, concise);
        if tokens + cost > budget {
            break;
        }
        tokens += cost;
        taken += 1;
    }
    taken
}

/// The ONE place the `context` truncation remedy is worded.
///
/// nw-259(a). Two readers need this sentence — the `--stats` line and the
/// human renderer — and a third (`--json`) needs the same DECISION in
/// machine-readable form. The decision is made once, by
/// `TruncationCause::resolve`, and lands in `ContextResult::truncated_by`;
/// this function is the only thing that turns that value into prose. A second
/// copy of either half is how the human route came to name the right cap while
/// the JSON payload named the wrong one.
///
/// Returns `None` when nothing was cut — and also when the cause is known but
/// the cap's VALUE is not, because a remedy that cannot name the number to
/// raise is not a remedy.
pub(crate) fn context_truncation_notice(
    truncated_by: Option<nestweaver_engine::TruncationCause>,
    limit: Option<usize>,
    token_budget: Option<usize>,
) -> Option<String> {
    match (truncated_by?, limit, token_budget) {
        (nestweaver_engine::TruncationCause::TokenBudget, _, Some(budget)) => Some(format!(
            "TRUNCATED by --token-budget {budget} — raise it for more"
        )),
        (nestweaver_engine::TruncationCause::Limit, Some(limit), _) => Some(format!(
            "TRUNCATED at limit {limit} — pass --limit for more"
        )),
        _ => None,
    }
}

pub(crate) fn context_token_budgeted_truncate(
    connected: &[nestweaver_engine::ContextNode],
    budget: usize,
) -> usize {
    let mut tokens = 0usize;
    let mut taken = 0usize;
    for n in connected {
        let cost = (n.uid.len()
            + n.name.len()
            + n.kind.len()
            + n.file_path.len()
            + n.signature.len()
            + 20)
            .div_ceil(4);
        if tokens + cost > budget {
            break;
        }
        tokens += cost;
        taken += 1;
    }
    taken
}

/// Apply age-decay score boost to non-Symbol nodes (CLI variant).
pub(crate) fn apply_recency_bias_cli(
    store: &nestweaver_store::GraphStore,
    nodes: &mut [nestweaver_engine::BrainNode],
    recency_weight: f64,
    recency_half_life_days: f64,
) {
    if recency_weight <= 0.0 {
        return;
    }
    let note_timestamps: std::collections::HashMap<String, f64> = store
        .list_notes(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| {
            n.modified_at
                .map(|t| (n.uid, nestweaver_engine::parse_iso8601_to_epoch(&t)))
        })
        .collect();
    let section_note_map: std::collections::HashMap<String, String> = store
        .list_all_sections()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.uid, s.note_uid))
        .collect();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    let ln2 = std::f64::consts::LN_2;
    let half_life_secs = recency_half_life_days * 86_400.0;

    for node in nodes.iter_mut() {
        if node.kind.to_lowercase().contains("symbol") {
            continue;
        }
        let modified_at_secs = if let Some(&ts) = note_timestamps.get(&node.uid) {
            ts
        } else if let Some(note_uid) = section_note_map.get(&node.uid) {
            note_timestamps.get(note_uid).copied().unwrap_or(0.0)
        } else {
            0.0
        };
        if modified_at_secs <= 0.0 {
            continue;
        }
        let age_secs = (now - modified_at_secs).max(0.0);
        let boost = 1.0 + recency_weight * (-(age_secs * ln2) / half_life_secs).exp();
        node.relevance *= boost;
    }

    nodes.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Rough token cost of rendering a single BrainNode line (chars / 4).
/// Aligned with the MCP render_cost to avoid CLI vs MCP divergence.
/// Estimated token cost of one rendered node.
///
/// nw-316: this was a COPY of `nestweaver_mcp::tools::render_cost`'s
/// `concise == false` branch, unconditionally — and `project-context` defaults
/// to CONCISE, whose renderer emits only `{kind, title, location}`. So the
/// budget charged roughly `(uid.len() + 40) / 4` tokens per node that the
/// renderer never spent, and took fewer nodes than the budget allowed. That is
/// the reported 14-items-vs-20.
///
/// It now delegates rather than mirroring both branches: a copy that agrees
/// with the original is still a copy, and this one drifted from it precisely
/// because it could.
pub(crate) fn render_cost_tokens(n: &nestweaver_engine::BrainNode, concise: bool) -> usize {
    nestweaver_mcp::tools::render_cost(n, concise)
}

/// Render a `clusters` result in either mode.
///
/// Shared so the cached and freshly-computed paths cannot drift (nw-075): the
/// whole point of serving a cache is that the caller cannot tell the difference.
/// Bound a clustering result for output, reporting what was dropped.
///
/// nw-182: `clusters --json` emitted 65 MB with no `--limit` flag at all —
/// output proportional to the CORPUS, not to the result. It is also exposed as
/// an MCP tool, where that is a context-window bomb rather than an answer. The
/// MCP tool already truncated member lists at 20; the CLI did not.
///
/// Truncation is reported rather than silent: `total` vs `returned`
/// (`clusters` array, nw-559 names), and a per-cluster `returned_members`, so a caller
/// can tell a small graph from a truncated view. `0` means unlimited for both
/// bounds, so the previous full output is still reachable.
/// `graph_generation`/`cached` are nw-646's cache-identity disclosure: before
/// this, "was this cache hit or a fresh compute, and against which graph
/// generation" was ONLY in the human-readable stderr status line
/// ("Using cached clusters (resolution=…, generation=…)"), which `--json`
/// callers (scripts, the MCP client) cannot read. `graph_generation` is
/// `None` only for the daemon-routed text path, which never reaches `--json`
/// (see the call site's comment) and so never serializes this payload.
pub(crate) fn bounded_clusters_payload(
    output: &nestweaver_engine::ClusteringOutput,
    limit: usize,
    members: usize,
    graph_generation: Option<u64>,
    cached: bool,
) -> serde_json::Value {
    // nw-559: the MCP `clusters` envelope, built by the same function, so
    // `clusters --json` and the tool cannot drift on key names again.
    nestweaver_mcp::tools::clusters_payload(output, limit, members, graph_generation, cached, None)
}

/// Render the text form of a clustering result, bounded to `limit`.
///
/// Returns a `String` rather than printing so the bound is assertable without
/// capturing stdout, and so BOTH routes can share one renderer — the daemon
/// branch of `Commands::Clusters` used to carry its own inline copy of this
/// loop with the bounding removed, which is how `--limit` came to have no
/// effect there at all (nw-299b).
///
/// `total` is passed in rather than read from `output.communities.len()`
/// because on the daemon route the list has ALREADY been cut server-side, so a
/// total taken from what arrived would report the cap as the population — the
/// same lie F-DC-11 records for `summary --level cluster`.
pub(crate) fn render_clusters_text(
    output: &nestweaver_engine::ClusteringOutput,
    limit: usize,
    total: usize,
) -> String {
    use std::fmt::Write as _;

    if output.communities.is_empty() {
        return "No communities detected (graph may be empty or fully disconnected).\n".to_string();
    }
    let shown = output.communities.len();
    let take = if limit == 0 { shown } else { limit.min(shown) };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Clusters ({total}, modularity={:.4}):\n",
        output.modularity
    );
    for c in &output.communities[..take] {
        let _ = writeln!(
            out,
            "  [{:>3}] {} ({} members, cohesion={:.2})",
            c.id, c.name, c.member_count, c.cohesion
        );
        for f in &c.key_files {
            let _ = writeln!(out, "        {f}");
        }
    }
    if take < total {
        let _ = writeln!(
            out,
            "\n  … {} more community(ies) not shown — raise --limit (0 = all)",
            total - take
        );
    }
    out
}

/// Render a `clusters --repo` payload (the `clusters` tool's scoped branch)
/// as text.
///
/// nw-479. Deliberately a separate renderer, not a patch onto
/// [`render_clusters_text`]: the scoped payload's JSON shape (`size` rather
/// than `member_count`, plus a `scope` object) is the tool's wire shape, not
/// the direct-path `ClusteringOutput`/`CommunityInfo` structs, so there is no
/// shared struct to render from without first reconstructing one — and the
/// `scope` disclosure (repos, cross-repo edges cut, and the `repo_scoped` id
/// space warning) has nothing to share with the unscoped renderer anyway.
pub(crate) fn render_scoped_clusters_text(payload: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let modularity = payload
        .get("modularity")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let total = payload.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let scope = payload.get("scope");
    let scope_repos: Vec<String> = scope
        .and_then(|s| s.get("repos"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let excluded = scope
        .and_then(|s| s.get("cross_repo_edges_excluded"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Clusters scoped to [{}] ({total}, modularity={modularity:.4}); {excluded} cross-repo edge(s) excluded by the induced subgraph.",
        scope_repos.join(", ")
    );
    let _ = writeln!(
        out,
        "Community ids are in a SEPARATE 'repo_scoped' id space — not comparable to an unscoped run or another scope.\n"
    );
    if let Some(clusters) = payload.get("clusters").and_then(|v| v.as_array()) {
        if clusters.is_empty() {
            let _ = writeln!(
                out,
                "No communities detected (scope may be empty or fully disconnected)."
            );
        }
        for c in clusters {
            let id = c.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let size = c.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let cohesion = c.get("cohesion").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let _ = writeln!(
                out,
                "  [{id:>3}] {name} ({size} members, cohesion={cohesion:.2})"
            );
            if let Some(key_files) = c.get("key_files").and_then(|v| v.as_array()) {
                for f in key_files.iter().filter_map(|v| v.as_str()) {
                    let _ = writeln!(out, "        {f}");
                }
            }
        }
    }
    out
}

pub(crate) fn print_clusters_output(
    output: &nestweaver_engine::ClusteringOutput,
    json: bool,
    limit: usize,
    members: usize,
    graph_generation: u64,
    cached: bool,
) -> anyhow::Result<()> {
    // The direct path holds the whole population, so the total IS the length.
    let total = output.communities.len();
    print_clusters_output_with_total(
        output,
        json,
        limit,
        members,
        total,
        Some(graph_generation),
        cached,
    )
}

/// [`print_clusters_output`] with the pre-cap total supplied by the caller, for
/// the route that no longer has it (nw-299b).
/// Build the `clusters` tool arguments for the daemon route.
///
/// CLAMPED, not forwarded verbatim. `--limit` and `--members` are unbounded
/// `usize` in clap, while the tool caps limit at 1000 and members at 200 — two
/// DIFFERENT ceilings — and under `additionalProperties: false` an
/// out-of-range value fails the WHOLE call rather than being clamped
/// server-side, so `clusters` would stop working on the daemon route for
/// anyone who passed a large bound.
///
/// Clamping is strictly better than not forwarding at all, in every case. An
/// OMITTED key is not "unbounded" — `read_limit` applies the tool's default of
/// 50 — so declining to forward `--limit 200` was already substituting a
/// SMALLER bound than the caller asked for, and the printer could not restore
/// the 150 communities that never arrived.
///
/// `0` is the spelling of "all" on both sides and survives the clamp untouched.
/// The ceilings are imported rather than restated: a second copy of a bound
/// that has already drifted once is how these routes diverge.
pub(crate) fn clusters_tool_args(
    limit: usize,
    members: usize,
    resolution: Option<f64>,
) -> serde_json::Value {
    fn clamp(requested: usize, ceiling: usize) -> usize {
        if requested == 0 {
            0
        } else {
            requested.min(ceiling)
        }
    }
    let mut args = serde_json::json!({
        "limit": clamp(limit, nestweaver_mcp::tools::CLUSTERS_LIMIT_MAX),
        "members": clamp(members, nestweaver_mcp::tools::CLUSTERS_MEMBERS_MAX),
    });
    if let Some(r) = resolution {
        args["resolution"] = serde_json::json!(r);
    }
    args
}

/// Apply `dead-code`'s result cut on the direct route, and its DEFAULT cut.
///
/// Returns `(shown, truncated)`; the caller keeps the pre-cut count as the
/// reported total.
///
/// Round 3 rewrote this flag's help from "default: all" to "default 50",
/// because 50 is what the `dead_code` TOOL applies, and stopped there. The
/// direct arm still read `match limit { None => everything }`, so the two
/// routes answered the same bare `dead-code` differently: 50 rows and
/// `truncated: true` with a daemon up, every row and `truncated: false` when
/// the daemon was unreachable and the fallback ran. Help that is true on only
/// one transport is the nw-357 shape, one command over — and `truncated` was
/// the field that existed to disclose it.
///
/// `DEFAULT_RESULT_LIMIT` rather than [`resolve_limit`]: `dead-code` takes no
/// `--config`, so there is no instance config to consult on this side. An
/// operator's `[limits].default_result_limit` still applies on the daemon
/// route, where the daemon reads its own — which is what the help's "or
/// [limits].default_result_limit from config" clause names, and why a value is
/// NOT synthesised here and sent, which would override it.
#[cfg(test)]
pub(crate) fn dead_code_cut<T>(rows: Vec<T>, limit: Option<usize>) -> (Vec<T>, bool) {
    let total = rows.len();
    let effective = limit.unwrap_or(nestweaver_engine::config::DEFAULT_RESULT_LIMIT);
    (
        rows.into_iter().take(effective).collect(),
        effective < total,
    )
}

/// Build the `get_summary` tool arguments for `summary`'s daemon route.
///
/// `token_budget` is sent UNCONDITIONALLY, including `0`.
///
/// `--token-budget` documents "0 = unlimited" and the direct path honours it.
/// The daemon route used to omit the key when the budget was 0, and the tool's
/// default for an ABSENT key is `SUMMARY_DEFAULT_TOKEN_BUDGET` (20000) — so on
/// the route that serves text output by default, asking for unlimited silently
/// got the TIGHTEST bound instead of none, and `--help` said the opposite. The
/// tool's schema is `minimum: 0` and its own comment already reads "`0` still
/// means unlimited on both"; the CLI simply never sent the value that would
/// have made that true.
///
/// A function rather than an inline `json!`, for the same reason
/// [`clusters_tool_args`] is one: the omission is only assertable if the
/// arguments can be built without a daemon.
pub(crate) fn summary_tool_args(
    level: &str,
    token_budget: usize,
    target: Option<&str>,
) -> serde_json::Value {
    let mut args = serde_json::json!({ "level": level, "token_budget": token_budget });
    if let Some(t) = target {
        args["target"] = serde_json::json!(t);
    }
    args
}

pub(crate) fn print_clusters_output_with_total(
    output: &nestweaver_engine::ClusteringOutput,
    json: bool,
    limit: usize,
    members: usize,
    total: usize,
    graph_generation: Option<u64>,
    cached: bool,
) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&bounded_clusters_payload(
                output,
                limit,
                members,
                graph_generation,
                cached
            ))?
        );
        return Ok(());
    }
    print!("{}", render_clusters_text(output, limit, total));
    Ok(())
}

/// Header line for `brain context` seeds.
///
/// nw-102: `seeds` mixes nodes resolved from the query text with nearest
/// neighbours injected by the semantic leg, and counting both as "resolved"
/// made the output contradict itself — a nonsense query printed
/// `Seeds (5 resolved)` while the same response listed that query under
/// `Unresolved seeds (1)`. Reports PROVENANCE, not quality: a phrase like
/// "blast radius" fails a direct lookup against the symbol `blast_radius` yet
/// its semantic hits are excellent, so labelling them guesses would understate
/// them exactly as counting them as direct matches overstated them.
///
/// nw-393: the header also carries the SEED-RESOLUTION disclosure, because
/// `Seeds (5 resolved)` for a name that matched 200 symbols is
/// indistinguishable from a name that genuinely has five definitions. The
/// machine payloads (`seed_matches_total`, `seed_matches_total_relation`,
/// `seeds_truncated`, `seed_resolution_limit`) already say the set was cut; a
/// human line that does not is the human-vs-machine split nw-259(a) ruled
/// against — the prose must not claim something the payload contradicts.
/// Shared with `print_context_text` rather than given a second spelling: two
/// surfaces drifting apart about the same query is the failure mode being
/// fixed, not a shape to reproduce.
pub(crate) fn seed_header(
    total_seeds: usize,
    semantic_seeds: usize,
    seed_matches_total: Option<usize>,
    seed_matches_total_relation: Option<&str>,
    seeds_truncated: Option<bool>,
) -> String {
    let matched = total_seeds.saturating_sub(semantic_seeds);
    // Annotate ONLY when the cap actually BIT. `Some(false)` — a name with
    // exactly `SEED_NAME_MATCH_LIMIT` definitions, all of them resolved — and
    // `None` — no bare-name input reached symbol search at all, e.g. a `sym:`
    // UID or note-title seed, where the cap was never in force — both leave
    // the line byte-identical to before. "5 of 5" is noise and "0 of 0" is a
    // false cut claim about an exhaustive seed form; either one trains the
    // reader to skip the clause that matters.
    let (of_total, cap) = if seeds_truncated == Some(true) {
        let of_total = match (seed_matches_total, seed_matches_total_relation) {
            // `gte` means the counted search stopped at
            // `SYMBOL_SEARCH_COUNT_CAP`, so the number is a LOWER BOUND and
            // printing it as exact would replace one false precision with
            // another. Same wording `brain search` already uses for a `gte`
            // total ("N of at least M result(s)"), so there is one human
            // spelling of "this count is a floor", not two.
            (Some(total), Some("gte")) => format!(" of at least {total}"),
            (Some(total), _) => format!(" of {total}"),
            // Flag set, count absent: a producer that predates the count, or a
            // daemon payload that carried one field and not the other. Name the
            // cut — it is the load-bearing half — but do not invent a total.
            (None, _) => String::new(),
        };
        (of_total, ", seed-resolution cap")
    } else {
        (String::new(), "")
    };
    if semantic_seeds > 0 {
        // The cap bounds the DIRECT name-match leg only; semantic hits are
        // injected by KNN and are not subject to it, so the `of N` rides on the
        // "matched directly" count and never on the semantic one.
        format!(
            "Seeds ({matched}{of_total} matched directly, \
             {semantic_seeds} via semantic search{cap}):"
        )
    } else if of_total.is_empty() {
        format!("Seeds ({matched} resolved{cap}):")
    } else {
        format!("Seeds ({matched}{of_total}{cap}):")
    }
}

pub(crate) fn print_brain_context_text(
    result: &BrainContextResult,
    cut: usize,
    token_budget: Option<usize>,
    upstream: &UpstreamContextDisclosure,
) {
    if let Some(note) = publication_text_note(upstream.publication.as_ref()) {
        println!("{note}");
    }
    if let Some(pending) = &upstream.code_links {
        println!(
            "{}",
            nestweaver_engine::code_links::code_links_text_note(pending)
        );
    }
    if let Some(detail) = &result.semantic_unavailable {
        println!(
            "Warning: semantic retrieval unavailable ({}). {}",
            detail["reason"].as_str().unwrap_or("unknown"),
            detail["remediation"]
                .as_str()
                .unwrap_or("verify embedding readiness")
        );
        if let Some(pipeline) = detail.get("pipeline").filter(|v| !v.is_null()) {
            println!("  Pipeline: {pipeline}");
        }
    }
    // Feature F7: show PRF-mined expansion terms for auditing.
    if !result.expansion_terms.is_empty() {
        println!("PRF expansion terms: {}", result.expansion_terms.join(", "));
        println!();
    }
    // nw-102: `seeds` mixes two different things — nodes actually resolved from
    // the query, and nearest-neighbour guesses injected by the semantic leg,
    // which vector KNN returns regardless of how distant they are. Counting
    // both as "resolved" made the output contradict itself: a nonsense query
    // printed `Seeds (5 resolved)` while ALSO listing that same query under
    // `Unresolved seeds (1)`. Report the two separately.
    // nw-393 rides on the same line: the seed set is also cut by the
    // per-input name-match cap, and this route resolves seeds through its own
    // loop, so the disclosure has to be attached here too or `brain context`
    // and `context` would disagree about an identical query.
    println!(
        "{}",
        seed_header(
            result.seeds.len(),
            result.semantic_seed_count,
            result.seed_matches_total,
            result.seed_matches_total_relation.as_deref(),
            result.seeds_truncated,
        )
    );
    for n in &result.seeds {
        if n.location.is_empty() {
            println!("  {}  [{}]", n.title, n.kind);
        } else {
            println!("  {}  [{}]  {}", n.title, n.kind, n.location);
        }
    }

    if !result.unresolved_seeds.is_empty() {
        println!();
        println!("Unresolved seeds ({}):", result.unresolved_seeds.len());
        for s in &result.unresolved_seeds {
            println!("  {s}");
        }
        if result.seeds.len() == result.semantic_seed_count && result.semantic_seed_count > 0 {
            // State HOW the seeds were found, not how good they are. A phrase
            // like "blast radius" fails a direct lookup against the symbol
            // `blast_radius` yet its semantic hits are excellent, so calling
            // them guesses would understate them just as counting them as
            // direct matches overstated them.
            println!(
                "  note: no seed matched the query text directly; the seeds above \
                 came from semantic similarity."
            );
        }
    }

    if !result.connected.is_empty() {
        println!();
        // Same correction as the JSON twin: on the daemon route
        // `result.connected` is post-cut, so `Connected (N of M)` printed
        // `N of N` and the HUMAN route lost the disclosure it has had all along.
        let total = upstream.total(result.connected.len(), cut);
        let used_tokens: usize = result
            .connected
            .iter()
            .take(cut)
            // Text renderer: prints the full record, so the detailed rate is
            // the right one. Unchanged from before nw-316.
            .map(|n| render_cost_tokens(n, false))
            .sum();
        match token_budget {
            Some(budget) => println!(
                "Connected ({} of {}, ~{}/{} tokens, ranked by relevance):",
                cut, total, used_tokens, budget
            ),
            None => println!("Connected ({} of {}, ranked by relevance):", cut, total),
        }
        for n in result.connected.iter().take(cut) {
            if n.location.is_empty() {
                println!("  {:.4}  {}  [{}]", n.relevance, n.title, n.kind);
            } else {
                println!(
                    "  {:.4}  {}  [{}]  {}",
                    n.relevance, n.title, n.kind, n.location
                );
            }
            if let Some(body) = &n.inline_body {
                for line in body.lines() {
                    println!("      | {line}");
                }
            }
        }
    }
}

/// Render a daemon-routed `BrainSearchResponse` in the same shape as the
/// direct-disk `brain search` handler. The daemon merges note + symbol
/// results into a single `results` array, distinguished by `kind`
/// (`"note"` vs `"Symbol/<Kind>"`); text mode splits them back out so the
/// per-row format matches the legacy output.
pub(crate) struct BrainSearchDisplayMetadata<'a> {
    pub(crate) returned_matches: i32,
    pub(crate) total_matches_relation: &'a str,
    pub(crate) truncated: bool,
}

pub(crate) fn brain_search_display_metadata(
    response: &nestweaver_proto::BrainSearchResponse,
) -> BrainSearchDisplayMetadata<'_> {
    let returned_matches = if response.returned_matches == 0 && !response.results.is_empty() {
        response.results.len() as i32
    } else {
        response.returned_matches
    };
    let total_matches_relation = if response.total_matches_relation.is_empty() {
        "gte"
    } else {
        &response.total_matches_relation
    };
    let truncated = response.truncated
        || total_matches_relation != "eq"
        || returned_matches < response.total_matches;
    BrainSearchDisplayMetadata {
        returned_matches,
        total_matches_relation,
        truncated,
    }
}

pub(crate) fn brain_search_engine_header(engine: &str) -> &'static str {
    match engine {
        "bm25" => "Brain search (BM25)",
        "hybrid" => "Brain search (hybrid)",
        "substring" => "Brain search (substring fallback)",
        _ => "Brain search",
    }
}

pub(crate) fn brain_search_result_item_json(
    item: &nestweaver_proto::SearchResultItem,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "uid": item.uid,
        "kind": item.kind,
        "title": item.title,
        "score": item.score,
    });
    if let Some(ref canonical_id) = item.canonical_id {
        value["canonical_id"] = serde_json::json!(canonical_id);
    }
    if let Some(ref location) = item.location {
        value["location"] = serde_json::json!(location);
    }
    if !item.matched_headings.is_empty() {
        value["matched_headings"] = serde_json::json!(item.matched_headings);
    }
    if let Some(ref vault_uid) = item.vault_uid {
        value["vault_uid"] = serde_json::json!(vault_uid);
    }
    if let Some(ref body) = item.inline_body {
        value["inline_body"] = serde_json::json!(body);
    }
    value
}

pub(crate) fn semantic_unavailable_json(
    detail: &nestweaver_proto::SemanticUnavailable,
) -> serde_json::Value {
    serde_json::json!({
        "cause": detail.cause,
        "reason": detail.reason,
        "error": detail.error,
        "remediation": detail.remediation,
    })
}

pub(crate) fn render_brain_search_response(
    resp: &nestweaver_proto::BrainSearchResponse,
    json: bool,
) -> anyhow::Result<()> {
    let metadata = brain_search_display_metadata(resp);
    let returned_matches = metadata.returned_matches;
    let total_matches_relation = metadata.total_matches_relation;
    let truncated = metadata.truncated;
    if json {
        let results: Vec<serde_json::Value> = resp
            .results
            .iter()
            .map(brain_search_result_item_json)
            .collect();
        let mut payload = serde_json::json!({
            "query": resp.query,
            "engine": resp.engine,
            "engine_warning": resp.engine_warning,
            "limit_per_kind": resp.limit_per_kind,
            "results": results,
            "total_matches": resp.total_matches,
            "total_matches_relation": total_matches_relation,
            "returned_matches": returned_matches,
            "truncated": truncated,
            "semantic_applied": resp.semantic_applied,
            "degraded_components": &resp.degraded_components,
        });
        if !resp.expansion_terms.is_empty() {
            payload["expansion_terms"] = serde_json::json!(resp.expansion_terms);
        }
        if let Some(detail) = &resp.semantic_unavailable {
            payload["semantic_unavailable"] = semantic_unavailable_json(detail);
        }
        print_json_payload(&payload)?;
        return Ok(());
    }

    if let Some(warning) = &resp.engine_warning {
        println!("Warning: {warning}");
    }
    if let Some(detail) = &resp.semantic_unavailable {
        println!(
            "Warning: semantic search unavailable ({}): {}\n  Remediation: {}",
            detail.reason, detail.error, detail.remediation
        );
    }

    if resp.results.is_empty() {
        println!("No results for '{}'.", resp.query);
        return Ok(());
    }

    let header = brain_search_engine_header(&resp.engine);
    if truncated {
        if total_matches_relation == "gte" {
            println!(
                "{}: {} of at least {} result(s)",
                header, returned_matches, resp.total_matches
            );
        } else {
            println!(
                "{}: {} of {} result(s)",
                header, returned_matches, resp.total_matches
            );
        }
    } else {
        println!("{}: {} result(s)", header, returned_matches);
    }
    if !resp.expansion_terms.is_empty() {
        println!("  PRF expansion terms: {}", resp.expansion_terms.join(", "));
    }
    println!();
    for item in &resp.results {
        if item.kind == "note" {
            if item.matched_headings.is_empty() {
                println!("  [{:.2}] {}", item.score, item.title);
            } else {
                println!(
                    "  [{:.2}] {} (matched: {})",
                    item.score,
                    item.title,
                    item.matched_headings.join(", "),
                );
            }
        } else {
            // Symbol/<Kind> row: split "kind" prefix off, render with location.
            let kind_short = item.kind.strip_prefix("Symbol/").unwrap_or(&item.kind);
            if let Some(ref loc) = item.location {
                println!(
                    "  [{:.2}] {} [{}] @ {}",
                    item.score, item.title, kind_short, loc,
                );
            } else {
                println!("  [{:.2}] {} [{}]", item.score, item.title, kind_short);
            }
        }
    }
    Ok(())
}

/// Render a brain search response from a JSON `Value` (hybrid path).
///
/// The JSON shape matches the proto `BrainSearchResponse` serialized by
/// `dispatch_typed_brain_search` in the hybrid client.
pub(crate) fn render_brain_search_json(result: &serde_json::Value) -> anyhow::Result<()> {
    let results = result
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let query = result.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let engine = result
        .get("engine")
        .and_then(|v| v.as_str())
        .unwrap_or("bm25");
    let expansion_terms: Vec<String> = result
        .get("expansion_terms")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let returned_matches = result
        .get("returned_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(results.len() as u64);
    let total_matches = result
        .get("total_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(returned_matches);
    let total_matches_relation = result
        .get("total_matches_relation")
        .and_then(|v| v.as_str())
        .unwrap_or("gte");
    let explicit_truncated = result
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let truncated =
        explicit_truncated || total_matches_relation != "eq" || returned_matches < total_matches;

    if let Some(warning) = result
        .get("engine_warning")
        .and_then(serde_json::Value::as_str)
    {
        println!("Warning: {warning}");
    }
    if let Some(warnings) = result
        .get("engine_warnings")
        .and_then(serde_json::Value::as_array)
    {
        for warning in warnings {
            println!(
                "Warning ({}): {}",
                warning["tier"].as_str().unwrap_or("upstream"),
                warning["warning"]
                    .as_str()
                    .unwrap_or("lexical backend unavailable")
            );
        }
    }
    if results.is_empty() {
        println!("No results for '{}'.", query);
        return Ok(());
    }

    let header = brain_search_engine_header(engine);
    // Include provenance scope if present (hybrid/local/server).
    let scope = result
        .get("_meta")
        .and_then(|m| m.get("scope"))
        .and_then(|v| v.as_str())
        .unwrap_or("local");
    if truncated {
        if total_matches_relation == "gte" {
            println!(
                "{}: {} of at least {} result(s) [{}]",
                header, returned_matches, total_matches, scope
            );
        } else {
            println!(
                "{}: {} of {} result(s) [{}]",
                header, returned_matches, total_matches, scope
            );
        }
    } else {
        println!("{}: {} result(s) [{}]", header, returned_matches, scope);
    }
    if !expansion_terms.is_empty() {
        println!("  PRF expansion terms: {}", expansion_terms.join(", "));
    }
    println!();
    for item in &results {
        let score = item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let location = item.get("location").and_then(|v| v.as_str());
        let matched_headings: Vec<&str> = item
            .get("matched_headings")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        if kind == "note" {
            if matched_headings.is_empty() {
                println!("  [{:.2}] {}", score, title);
            } else {
                println!(
                    "  [{:.2}] {} (matched: {})",
                    score,
                    title,
                    matched_headings.join(", "),
                );
            }
        } else {
            let kind_short = kind.strip_prefix("Symbol/").unwrap_or(kind);
            if let Some(loc) = location {
                println!("  [{:.2}] {} [{}] @ {}", score, title, kind_short, loc);
            } else {
                println!("  [{:.2}] {} [{}]", score, title, kind_short);
            }
        }
    }
    Ok(())
}

/// Declared-repo issues per materialized project name (nw-674).
pub(crate) type ProjectRepoIssues =
    std::collections::BTreeMap<String, Vec<nestweaver_engine::ProjectRepoIssue>>;

/// Member repos per materialized project name.
pub(crate) type ProjectMemberRepos =
    std::collections::BTreeMap<String, Vec<nestweaver_engine::ProjectMemberRepo>>;

/// The whole `list-projects` stdout, JSON or text. Extracted from the command
/// arm so the nw-674 disclosure in both formats is unit-testable.
pub(crate) fn render_list_projects(
    materialized: &[nestweaver_schema::Project],
    declared_only: &[nestweaver_engine::ProjectConfig],
    repo_issues: &ProjectRepoIssues,
    member_repos: &ProjectMemberRepos,
    json: bool,
) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    if json {
        #[derive(serde::Serialize)]
        struct ListProjectsJson<'a> {
            materialized: &'a [nestweaver_schema::Project],
            /// Every materialized project, keyed by name: its member repos
            /// (`[]` when it has none).
            member_repos: &'a ProjectMemberRepos,
            #[serde(skip_serializing_if = "<[nestweaver_engine::ProjectConfig]>::is_empty")]
            declared: &'a [nestweaver_engine::ProjectConfig],
            #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
            repo_issues: &'a ProjectRepoIssues,
        }
        let mut members = member_repos.clone();
        for project in materialized {
            members.entry(project.name.clone()).or_default();
        }
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&ListProjectsJson {
                materialized,
                member_repos: &members,
                declared: declared_only,
                repo_issues,
            })?
        ));
    }
    let mut out = String::new();
    if materialized.is_empty() && declared_only.is_empty() {
        writeln!(
            out,
            "No projects found. Use an instance config with [[projects]] to define them."
        )?;
        return Ok(out);
    }
    for p in materialized {
        writeln!(out, "{}", p.name)?;
        writeln!(out, "  UID:      {}", p.uid)?;
        writeln!(out, "  Instance: {}", p.instance_id)?;
        if let Some(ref summary) = p.summary {
            writeln!(out, "  Summary:  {summary}")?;
        }
        let members = member_repos.get(&p.name).map_or(&[][..], Vec::as_slice);
        if members.is_empty() {
            writeln!(out, "  Repos:    (none)")?;
        } else {
            let names: Vec<&str> = members.iter().map(|repo| repo.name.as_str()).collect();
            writeln!(out, "  Repos:    {}", names.join(", "))?;
        }
        let issues = repo_issues.get(&p.name).map_or(&[][..], Vec::as_slice);
        for line in nestweaver_engine::repo_issue_warning_lines(issues) {
            writeln!(out, "  {line}")?;
        }
        writeln!(out)?;
    }
    if !declared_only.is_empty() {
        writeln!(out, "Declared in config (not yet materialized):")?;
        for pc in declared_only {
            writeln!(out, "  {}", pc.name)?;
            if let Some(ref desc) = pc.description {
                writeln!(out, "    {desc}")?;
            }
        }
        writeln!(out)?;
    }
    Ok(out)
}

/// nw-674: the declared-repo warning lines `project-context` text output
/// prints under the project header, decoded from the tool response.
pub(crate) fn project_context_repo_issue_lines(value: &serde_json::Value) -> Vec<String> {
    let issues: Vec<nestweaver_engine::ProjectRepoIssue> = value
        .get("repo_issues")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    nestweaver_engine::repo_issue_warning_lines(&issues)
}

/// Render the daemon's `project_context` JSON response (shape produced by
/// `tool_project_context` in nestweaver-mcp). When `json` is true, emit the
/// response verbatim; otherwise print a project header followed by the
/// connected nodes.
pub(crate) fn render_project_context_daemon_response(
    value: &serde_json::Value,
    json: bool,
    token_budget: usize,
) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("warning: failed to serialize daemon response: {e}"),
        }
        return;
    }
    if let Some(detail) = value.get("semantic_unavailable").filter(|v| !v.is_null()) {
        println!("Warning: semantic retrieval unavailable: {detail}");
    }
    let project = value.get("project").and_then(|v| v.as_str()).unwrap_or("");
    let project_uid = value
        .get("project_uid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let used = value
        .get("tokens_used")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("Project: {project}  ({project_uid})");
    if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
        println!("  {note}");
    }
    // nw-674: declared repos that are NOT members must not read as members
    // that merely ranked low.
    for line in project_context_repo_issue_lines(value) {
        println!("  {line}");
    }
    println!();
    let empty = vec![];
    let connected = value
        .get("connected")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    println!("Connected ({} item(s)):", connected.len());
    for n in connected {
        let title = n.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let location = n.get("location").and_then(|v| v.as_str()).unwrap_or("");
        // concise responses omit `relevance` — don't print a fake [0.0000] for them.
        match n.get("relevance").and_then(|v| v.as_f64()) {
            Some(rel) => println!("  [{rel:.4}] {kind}  {title}  @{location}"),
            None => println!("  {kind}  {title}  @{location}"),
        }
    }
    println!();
    println!("Tokens used: {used} / budget: {token_budget}");
}

/// What an upstream already decided about a `brain_context` answer.
///
/// nw-353 follow-up. The daemon serves this command by running the very
/// `tool_brain_context` the MCP route runs, so its reply ALREADY carries the
/// canonical `{returned, total, truncated, truncated_by}` — computed on the
/// pre-cut list, which is the only place it can be computed correctly.
/// `BrainContextResult` declares no field for any of it, so
/// `serde_json::from_value` silently dropped it and both renderers re-derived
/// `total` from the rows that SURVIVED the daemon's cut.
///
/// That is why `returned < total` was structurally unreachable on the daemon
/// route while `--token-budget` swept 200..16000: the answer was being measured
/// against itself. The fact was never missing — it was on the wire and thrown
/// away at the boundary — so this carries it across rather than recomputing a
/// fifth copy of the triple.
///
/// `Default` is the DIRECT route, which builds the full list locally and can
/// still read its own pre-cut total off `result.connected`.
#[derive(Default)]
pub(crate) struct UpstreamContextDisclosure {
    pub(crate) truncated_by: Option<String>,
    /// Rows that matched upstream, BEFORE the cut it already applied.
    pub(crate) total: Option<usize>,
    /// Federation provenance (`_meta`) the hybrid layer attached: which
    /// sources answered, and whether any repo was stale. Dropping it told the
    /// caller a merged multi-repo answer was a local one.
    pub(crate) meta: Option<serde_json::Value>,
    /// nw-503: watcher-batch honesty keys. Captured from the wire before
    /// `BrainContextResult` decode drops them, or from the local marker on
    /// the direct route.
    pub(crate) publication: Option<nestweaver_engine::index_publication::WatcherBatchDisclosure>,
    /// nw-670 re-review R1: the `code_links_pending` object while note->code
    /// links are owed — from the wire (the daemon stamped it), else from the
    /// local sidecar on the direct route. Same class as nw-503: the typed
    /// decode would otherwise drop it.
    pub(crate) code_links: Option<serde_json::Value>,
}

impl UpstreamContextDisclosure {
    /// Read the disclosure off a hybrid/daemon payload before it is narrowed
    /// into `BrainContextResult`.
    pub(crate) fn from_wire(value: &serde_json::Value) -> Self {
        Self {
            truncated_by: value
                .get("truncated_by")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            total: value
                .get("total")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize),
            meta: value.get("_meta").cloned(),
            publication: publication_from_wire(value),
            code_links: code_links_from_wire(value),
        }
    }

    pub(crate) fn with_local_publication(mut self, db_path: &std::path::Path) -> Self {
        if self.publication.is_none() {
            self.publication =
                nestweaver_engine::index_publication::status(db_path).watcher_batch_disclosure();
        }
        if self.code_links.is_none() {
            self.code_links = nestweaver_engine::code_links::code_links_pending_json(db_path);
        }
        self
    }

    /// The pre-cut total, preferring the upstream's when it supplied one.
    ///
    /// Clamped to at least `returned`: a payload claiming fewer matches than
    /// rows it just handed over is self-contradictory, and letting that through
    /// would publish `returned > total` — a shape no consumer should have to
    /// parse.
    fn total(&self, local_total: usize, returned: usize) -> usize {
        self.total.unwrap_or(local_total).max(returned)
    }
}

pub(crate) fn print_brain_context_json(
    result: &BrainContextResult,
    limit: usize,
    token_budget: Option<usize>,
    upstream: &UpstreamContextDisclosure,
) -> anyhow::Result<()> {
    let resp = brain_context_json_value(result, limit, token_budget, upstream);
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

/// nw-353. `limit` is the cut the caller already computed; `result.connected`
/// is the PRE-cut list, so the total is right here behind the `.take()` and
/// used to be thrown away. `print_brain_context_text` one function over has
/// printed `Connected (N of M, ...)` all along, so the machine route was the
/// only audience that could not tell a capped answer from a complete one.
///
/// `token_budget` is a parameter rather than re-derived because this function
/// could not name a cause without it — and naming the wrong cap sends the
/// caller to a knob that cannot help, which is the whole of nw-259.
pub(crate) fn brain_context_json_value(
    result: &BrainContextResult,
    limit: usize,
    token_budget: Option<usize>,
    upstream: &UpstreamContextDisclosure,
) -> serde_json::Value {
    let returned = limit.min(result.connected.len());
    // On the direct route `result.connected` IS the pre-cut list, so its length
    // is the total. On the daemon route it is what survived the daemon's cut,
    // and the honest total came over the wire.
    let total = upstream.total(result.connected.len(), returned);
    let truncated = returned < total;
    let budget_cut =
        token_budget.map(|budget| token_budgeted_truncate(&result.connected, budget, false));
    let (_, truncated_by) =
        nestweaver_engine::compose_result_caps(result.connected.len(), Some(limit), budget_cut);
    let mut resp = serde_json::json!({
        "seeds_expanded": result.seeds.len(),
        "connected": result.connected.iter().take(limit).collect::<Vec<_>>(),
        "returned": returned,
        "total": total,
        "truncated": truncated,
        // Emitted even when null, matching the MCP twin: one shape parses
        // both routes.
        "truncated_by": truncated_by.map(nestweaver_engine::TruncationCause::as_str).or(upstream.truncated_by.as_deref()),
        "semantic_applied": result.semantic_applied,
        "semantic_unavailable": result.semantic_unavailable,
        "degraded_components": result.degraded_components,
    });

    // nw-393. The SEED cap, one layer upstream of the connected-list disclosure
    // above. `seeds_expanded` alone cannot distinguish a name with five
    // definitions from a name with two hundred, and `truncated_by: "limit"`
    // MISDIRECTS here, because no caller-settable knob can recover a seed that
    // was never resolved.
    //
    // Independent fields rather than a `truncated_by: "seed_resolution"`
    // variant, deliberately: the two caps do NOT compose. `truncated_by:
    // "limit"` stays a correct, actionable statement about `connected` even
    // when the seed set was separately cut, so folding them into one scalar
    // would have to discard one true answer to state the other.
    //
    // Emitted only when the cap was IN FORCE (a uid-form seed is exhaustive and
    // reports nothing), so a "0 of 0" is never printed for a seed form the cap
    // cannot bound.
    if let Some(total) = result.seed_matches_total {
        resp["seed_matches_total"] = serde_json::json!(total);
    }
    if let Some(relation) = result.seed_matches_total_relation.as_deref() {
        resp["seed_matches_total_relation"] = serde_json::json!(relation);
    }
    if let Some(t) = result.seeds_truncated {
        resp["seeds_truncated"] = serde_json::json!(t);
    }
    if let Some(l) = result.seed_resolution_limit {
        resp["seed_resolution_limit"] = serde_json::json!(l);
    }

    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = serde_json::json!(result.unresolved_seeds);
    }
    if result.semantic_seed_count > 0 {
        resp["semantic_seed_count"] = serde_json::json!(result.semantic_seed_count);
    }

    // Feature F7: surface PRF-mined expansion terms for auditing.
    if !result.expansion_terms.is_empty() {
        resp["expansion_terms"] = serde_json::json!(result.expansion_terms);
    }

    // Federation provenance, passed through rather than regenerated — the CLI
    // is not the layer that knows which upstreams answered. Absent on the
    // direct route, which has exactly one source and never wrote a `_meta`.
    resp["_meta"] = upstream
        .meta
        .clone()
        .unwrap_or_else(|| nestweaver_schema::provenance::provenance("direct", &["direct"], &[]));
    if let Some(disclosure) = &upstream.publication {
        disclosure.stamp_into(&mut resp);
    }
    if let Some(pending) = &upstream.code_links {
        resp["code_links_incomplete"] = serde_json::json!(true);
        resp["code_links_pending"] = pending.clone();
    }
    resp
}

/// nw-670 re-review R1: the `code_links_pending` object a daemon answer
/// carried, if it said links are owed.
pub(crate) fn code_links_from_wire(value: &serde_json::Value) -> Option<serde_json::Value> {
    (value
        .get("code_links_incomplete")
        .and_then(serde_json::Value::as_bool)
        == Some(true))
    .then(|| {
        value
            .get("code_links_pending")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    })
}

pub(crate) fn publication_from_wire(
    value: &serde_json::Value,
) -> Option<nestweaver_engine::index_publication::WatcherBatchDisclosure> {
    if value
        .get("publication_in_progress")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    Some(
        nestweaver_engine::index_publication::WatcherBatchDisclosure {
            marker_age_s: value
                .get("marker_age_s")
                .and_then(serde_json::Value::as_u64),
            note_paths: value
                .get("in_flight_note_paths")
                .and_then(|paths| serde_json::from_value(paths.clone()).ok())
                .unwrap_or_default(),
            note_paths_truncated: value
                .get("in_flight_note_paths_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        },
    )
}

/// `brain doc-stats`'s text rendering, shared by the daemon and direct routes.
///
/// nw-554. The two unresolved-link counts are ONE population of broken links
/// deduplicated two ways (nw-345): distinct (note, target) and distinct
/// (section, target). Printed as two "unresolved links" lines, one missing
/// `[[Note]]` read as a note-level miss AND a section-level miss. They are now
/// one line: the vault-health number, then the finer per-section count in
/// parentheses. The JSON keys and their meanings are unchanged.
pub(crate) fn doc_stats_text_lines(stats: &nestweaver_engine::DocStats) -> Vec<String> {
    let mut lines = vec![
        "Document graph stats:".to_string(),
        format!("  total notes:      {}", stats.total_notes),
        format!(
            "  wikilink edges:                        {}",
            stats.wikilink_edges
        ),
        format!(
            "  unresolved links:                      {} (distinct note + target; {} counted per source section)",
            stats.unresolved_link_targets, stats.unresolved_link_section_targets
        ),
        format!(
            "  low-confidence (resolved, not broken): {}",
            stats.low_confidence_link_targets
        ),
        format!("  orphans:          {}", stats.orphans),
        format!("  avg out-degree:   {:.2}", stats.avg_outdegree),
    ];
    if !stats.top_tags.is_empty() {
        lines.push("  top tags:".to_string());
        for t in &stats.top_tags {
            lines.push(format!("    #{} ({})", t.tag, t.count));
        }
    }
    if !stats.notes_by_year.is_empty() {
        let mut years: Vec<(&String, &usize)> = stats.notes_by_year.iter().collect();
        years.sort_by(|a, b| a.0.cmp(b.0));
        lines.push("  notes by year:".to_string());
        for (year, count) in years {
            lines.push(format!("    {year}: {count}"));
        }
    }
    lines
}

#[cfg(test)]
mod doc_stats_text_tests {
    use super::doc_stats_text_lines;

    fn stats(targets: usize, section_targets: usize) -> nestweaver_engine::DocStats {
        serde_json::from_value(serde_json::json!({
            "total_notes": 1,
            "wikilink_edges": 0,
            "unresolved_link_targets": targets,
            "unresolved_link_section_targets": section_targets,
            "low_confidence_link_targets": 0,
            "orphans": 0,
            "avg_outdegree": 0.0,
            "top_tags": [],
            "notes_by_year": {},
        }))
        .unwrap()
    }

    /// nw-554. One missing `[[Note]]` is one broken link. The text must not
    /// list it under two "unresolved" headings as if a note-level miss and a
    /// section-level miss both happened.
    #[test]
    fn one_missing_note_link_is_reported_once() {
        let lines = doc_stats_text_lines(&stats(1, 1));
        let unresolved: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("unresolved"))
            .collect();
        assert_eq!(unresolved.len(), 1, "{lines:#?}");
        assert!(
            unresolved[0].contains("unresolved links:                      1 "),
            "{lines:#?}"
        );
    }

    /// Counterweight: when the per-section count genuinely differs (the same
    /// target linked from two sections of one note), both numbers still show.
    #[test]
    fn a_differing_per_section_count_is_still_shown() {
        let lines = doc_stats_text_lines(&stats(2, 3));
        let line = lines
            .iter()
            .find(|line| line.contains("unresolved links"))
            .unwrap();
        assert!(
            line.contains(" 2 (") && line.contains("3 counted per source section"),
            "{line}"
        );
    }
}

#[cfg(test)]
mod clusters_envelope_parity_tests {
    use super::bounded_clusters_payload;
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;
    use std::collections::BTreeSet;

    fn store() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        for uid in ["a0", "a1", "b0", "b1"] {
            store
                .insert_symbol(&Symbol {
                    uid: uid.to_string(),
                    name: uid.to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:x".to_string(),
                    file_path: format!("src/{uid}.rs"),
                    start_line: 1,
                    end_line: 1,
                    signature: format!("fn {uid}()"),
                    summary: None,
                    content_hash: format!("h_{uid}"),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        for (src, dst) in [("a0", "a1"), ("a1", "a0"), ("b0", "b1"), ("b1", "b0")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: dst.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 1.0,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        store
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<String> {
        value
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !key.starts_with('_'))
            .cloned()
            .collect()
    }

    /// nw-559. `clusters --json` and MCP `clusters` named the same data
    /// differently (`communities`/`total_communities`/`returned_communities`/
    /// `member_count` against `clusters`/`total`/`returned`/`size`). One
    /// envelope now: the same top-level and per-cluster keys on both surfaces.
    /// Counterweight: the member populations are still the same.
    #[test]
    fn cli_clusters_json_uses_the_mcp_clusters_envelope() {
        let store = store();
        let output = nestweaver_engine::compute_clusters(&store, 1.0).unwrap();
        let cli = bounded_clusters_payload(&output, 0, 0, Some(store.graph_generation()), false);
        let mcp = nestweaver_mcp::tools::dispatch(
            &store,
            None,
            "clusters",
            serde_json::json!({ "limit": 0, "members": 0, "resolution": 1.0 }),
            None,
        )
        .unwrap();

        assert_eq!(keys(&cli), keys(&mcp), "cli: {cli}\nmcp: {mcp}");
        let cli_clusters = cli["clusters"].as_array().expect("cli clusters array");
        let mcp_clusters = mcp["clusters"].as_array().expect("mcp clusters array");
        assert!(!cli_clusters.is_empty());
        assert_eq!(cli_clusters.len(), mcp_clusters.len());
        for (c, m) in cli_clusters.iter().zip(mcp_clusters) {
            assert_eq!(keys(c), keys(m));
            assert_eq!(c["size"], m["size"]);
        }
        let members = |clusters: &[serde_json::Value]| -> BTreeSet<String> {
            clusters
                .iter()
                .flat_map(|c| c["members"].as_array().unwrap().clone())
                .map(|m| m["uid"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(members(cli_clusters), members(mcp_clusters));
        assert_eq!(members(cli_clusters).len(), 4);
        for key in ["cluster_count", "total", "returned", "truncated"] {
            assert_eq!(cli[key], mcp[key], "{key}");
        }
    }
}

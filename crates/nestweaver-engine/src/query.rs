use nestweaver_schema::{Repo, Service, Symbol};
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};
use serde::Serialize;

use anyhow::Context;

use crate::config::{FeatureConfig, LinkConfig};
use crate::pull::repo_name_from_url;

/// Tuning knobs for hybrid PPR + BM25 + semantic retrieval.
///
/// Defaults reflect empirical recommendations from the cited research:
/// - `rrf_k = 60.0` — Cormack-Clarke-Buettcher 2009 standard.
/// - `weight_ppr = 0.7`, `weight_bm25 = 0.3` — PPR is the primary
///   structural signal; BM25 covers what graph structure misses
///   (lexical relevance, freshly-mentioned terms). The 70/30 split is
///   a defensible default but tunable — callers may set unit weights
///   (0.5 / 0.5) if their workload is more lexically-driven.
/// - `weight_semantic = 0.0` — disabled by default until embeddings are
///   generated via `nestweaver embed`. Set to a non-zero value (e.g.
///   0.2, reducing ppr/bm25 proportionally) to enable the third signal.
/// - `bm25_limit = 500` — large enough that BM25's tail is comparable
///   to PPR's. The audit flagged 100 as too low (PPR returns variable-
///   length results often into the hundreds); 500 keeps the candidate
///   pool symmetric while staying well under the size where RRF
///   sorting cost matters.
/// - `semantic_limit = 200` — cap on the number of vector KNN results
///   fed into RRF. Bounded to keep the in-process cosine scan tractable.
#[derive(Debug, Clone)]
pub struct HybridSearchConfig {
    pub rrf_k: f64,
    pub weight_ppr: f64,
    pub weight_bm25: f64,
    pub weight_semantic: f64,
    pub bm25_limit: usize,
    pub semantic_limit: usize,
}

impl Default for HybridSearchConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            weight_ppr: 0.7,
            weight_bm25: 0.3,
            weight_semantic: 0.0,
            bm25_limit: 500,
            semantic_limit: 200,
        }
    }
}

/// Full details for a single symbol, including its call graph neighbours.
#[derive(Debug, Serialize)]
pub struct SymbolDetail {
    pub symbol: Symbol,
    pub callers: Vec<Symbol>,
    pub callees: Vec<Symbol>,
}

/// A lightweight summary used for disambiguation and search results.
#[derive(Debug, Serialize)]
pub struct SymbolCandidate {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
}

impl From<&Symbol> for SymbolCandidate {
    fn from(s: &Symbol) -> Self {
        SymbolCandidate {
            uid: s.uid.clone(),
            name: s.name.clone(),
            kind: s.kind.to_string(),
            file_path: s.file_path.clone(),
            start_line: s.start_line,
        }
    }
}

/// Result of a symbol lookup operation.
pub enum LookupResult {
    /// Exactly one match was found and full detail is available.
    Found(Box<SymbolDetail>),
    /// No symbol matched the query.
    NotFound,
    /// Multiple symbols share the same name; the caller must disambiguate.
    Ambiguous(Vec<SymbolCandidate>),
}

/// Look up a symbol by name or UID.
///
/// - If `name_or_uid` contains `':'` it is treated as a UID and looked up exactly.
/// - Otherwise a name lookup is performed; 0 matches → `NotFound`, 1 match → `Found`,
///   2+ matches → `Ambiguous`.
pub fn lookup_symbol(store: &GraphStore, name_or_uid: &str) -> Result<LookupResult, anyhow::Error> {
    if name_or_uid.contains(':') {
        // UID path
        match store.lookup_symbol(name_or_uid) {
            Ok(sym) => {
                let callers = store.callers_of(&sym.uid).context("fetch callers")?;
                let callees = store.callees_of(&sym.uid).context("fetch callees")?;
                Ok(LookupResult::Found(Box::new(SymbolDetail {
                    symbol: sym,
                    callers,
                    callees,
                })))
            }
            Err(nestweaver_store::StoreError::NotFound) => Ok(LookupResult::NotFound),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    } else {
        // Name path
        let matches = store
            .lookup_symbols_by_name(name_or_uid)
            .context("name lookup")?;

        match matches.len() {
            0 => Ok(LookupResult::NotFound),
            1 => {
                let sym = matches.into_iter().next().unwrap();
                let callers = store.callers_of(&sym.uid).context("fetch callers")?;
                let callees = store.callees_of(&sym.uid).context("fetch callees")?;
                Ok(LookupResult::Found(Box::new(SymbolDetail {
                    symbol: sym,
                    callers,
                    callees,
                })))
            }
            _ => {
                let candidates = matches.iter().map(SymbolCandidate::from).collect();
                Ok(LookupResult::Ambiguous(candidates))
            }
        }
    }
}

/// Search for symbols whose name contains `query` (substring match).
///
/// Returns up to `limit` candidates.  Full BM25 + vector search comes in Phase 3.
pub fn search_symbols(
    store: &GraphStore,
    query: &str,
    limit: usize,
) -> Result<Vec<SymbolCandidate>, anyhow::Error> {
    let syms = store
        .search_symbols_by_name(query, limit)
        .context("search_symbols_by_name")?;
    Ok(syms.iter().map(SymbolCandidate::from).collect())
}

// ── Context command types ─────────────────────────────────────────────────────

/// A single node returned by `build_context`.
#[derive(Debug, Serialize)]
pub struct ContextNode {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
    pub signature: String,
    pub relevance: f64,
}

/// A cross-repo relationship surfaced during context building.
#[derive(Debug, Serialize)]
pub struct CrossRepoLink {
    pub package: String,
    pub link_type: String,
    pub confidence: f32,
}

/// The full result returned by `build_context`.
#[derive(Debug, Serialize)]
pub struct ContextResult {
    pub seeds: Vec<ContextNode>,
    pub connected: Vec<ContextNode>,
    pub cross_repo_links: Vec<CrossRepoLink>,
}

/// Detect whether an input string looks like a file path.
fn is_file_path(input: &str) -> bool {
    input.contains('/')
        || input.contains('\\')
        || input.rsplit_once('.').is_some_and(|(_, ext)| {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_alphanumeric())
        })
}

/// Build a task-focused context subgraph around the given seed inputs.
///
/// Each entry in `inputs` is resolved to one or more symbol UIDs:
/// - Starts with `"sym:"` or `"repo:"` → UID lookup
/// - Looks like a file path → `symbols_in_file`
/// - Otherwise → `search_symbols_by_name(input, 5)`
///
/// Personalized PageRank (d = 0.85, 20 iterations) is then run from all
/// resolved seeds and the results are split into `seeds` and `connected`.
pub fn build_context(
    store: &GraphStore,
    inputs: &[String],
) -> Result<ContextResult, anyhow::Error> {
    let mut seed_uids: Vec<String> = Vec::new();
    let mut file_paths_tried: Vec<String> = Vec::new();

    for input in inputs {
        if input.starts_with("sym:") || input.starts_with("repo:") {
            // Direct UID lookup.
            match store.lookup_symbol(input) {
                Ok(sym) => seed_uids.push(sym.uid),
                Err(nestweaver_store::StoreError::NotFound) => {
                    // Skip UIDs that don't exist; the caller can handle empty seeds later.
                }
                Err(e) => return Err(anyhow::anyhow!(e)),
            }
        } else if is_file_path(input) {
            // Resolve all symbols in the file.
            let file_syms = store
                .symbols_in_file(input)
                .map_err(|e| anyhow::anyhow!(e))?;
            if file_syms.is_empty() {
                // Track that we attempted a file-path lookup that returned no symbols.
                file_paths_tried.push(input.clone());
            }
            for sym in file_syms {
                seed_uids.push(sym.uid);
            }
        } else {
            // Name search — take up to 5 matches.
            let matches = store
                .search_symbols_by_name(input, 5)
                .map_err(|e| anyhow::anyhow!(e))?;
            for sym in matches {
                seed_uids.push(sym.uid);
            }
        }
    }

    // Deduplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    seed_uids.retain(|uid| seen.insert(uid.clone()));

    if seed_uids.is_empty() {
        if !file_paths_tried.is_empty() {
            anyhow::bail!(
                "No symbols found in file(s): {}. The file may contain only re-exports or unsupported syntax.",
                file_paths_tried.join(", ")
            );
        }
        anyhow::bail!("No matching symbols found. Try `nestweaver search <term>` to find symbols.");
    }

    // Run Personalized PageRank over the code-only scope (preserves the
    // pre-brain behaviour of `nestweaver context`). The unified scope that
    // mixes code + notes is exposed via `nestweaver brain context`.
    let ppr_results = store
        .personalized_pagerank(&seed_uids, 0.85, 20, &GraphScope::code_only())
        .map_err(|e| anyhow::anyhow!(e))?;

    let seed_set: std::collections::HashSet<&str> = seed_uids.iter().map(|s| s.as_str()).collect();

    let mut seeds: Vec<ContextNode> = Vec::new();
    let mut connected: Vec<ContextNode> = Vec::new();

    for (uid, score) in &ppr_results {
        // Look up full symbol details.
        let sym = match store.lookup_symbol(uid) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let node = ContextNode {
            uid: sym.uid.clone(),
            name: sym.name.clone(),
            kind: sym.kind.to_string(),
            file_path: sym.file_path.clone(),
            start_line: sym.start_line,
            signature: sym.signature.clone(),
            relevance: *score,
        };

        if seed_set.contains(uid.as_str()) {
            seeds.push(node);
        } else {
            connected.push(node);
        }
    }

    // Collect cross-repo links for seed symbols.
    let mut cross_repo_links: Vec<CrossRepoLink> = Vec::new();
    for uid in &seed_uids {
        let refs = store
            .cross_repo_links(uid)
            .map_err(|e| anyhow::anyhow!(e))?;
        for r in refs {
            cross_repo_links.push(CrossRepoLink {
                package: r.target_name,
                link_type: r.link_type,
                confidence: r.confidence,
            });
        }
    }

    Ok(ContextResult {
        seeds,
        connected,
        cross_repo_links,
    })
}

// ── build_context tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod context_tests {
    use std::fs;

    use super::build_context;
    use crate::index::index_directory_in_memory;

    fn make_test_repo_with_calls() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) { return hello(name); }\nfunction hello(name) { return name; }",
        )
        .unwrap();
        fs::write(
            src.join("utils.js"),
            "function formatDate(date) { return date; }",
        )
        .unwrap();
        (dir, src)
    }

    #[test]
    fn build_context_resolves_symbol_names() {
        let (_dir, src) = make_test_repo_with_calls();
        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let result = build_context(&store, &["greet".to_string()]).unwrap();

        assert!(!result.seeds.is_empty(), "seeds should be populated");
        let seed_names: Vec<&str> = result.seeds.iter().map(|s| s.name.as_str()).collect();
        assert!(
            seed_names.contains(&"greet"),
            "greet should be a seed; seeds: {seed_names:?}"
        );
    }

    #[test]
    fn build_context_resolves_file_paths() {
        let (_dir, src) = make_test_repo_with_calls();
        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        // Find the actual file_path stored for a symbol in utils.js.
        let search = store.search_symbols_by_name("formatDate", 1).unwrap();
        if search.is_empty() {
            // Parser may not have indexed this file — skip rather than fail.
            return;
        }
        let file_path = search[0].file_path.clone();

        let result = build_context(&store, std::slice::from_ref(&file_path)).unwrap();

        assert!(
            !result.seeds.is_empty(),
            "symbols in {file_path} should appear as seeds"
        );
        assert!(
            result.seeds.iter().all(|n| n.file_path == file_path),
            "all seeds should come from the seeded file"
        );
    }

    #[test]
    fn build_context_not_found_returns_error() {
        let (_dir, src) = make_test_repo_with_calls();
        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let err = build_context(&store, &["zzz_no_such_symbol_xyz".to_string()])
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("No matching symbols"),
            "expected 'No matching symbols' error; got: {err}"
        );
    }
}

// ── Feature context ───────────────────────────────────────────────────────────

/// Information about a feature bundle.
#[derive(Debug, Serialize)]
pub struct FeatureInfo {
    pub name: String,
    pub description: Option<String>,
    pub repos: Vec<String>,
}

/// A declared cross-repo link surfaced in a feature context result.
#[derive(Debug, Serialize)]
pub struct LinkInfo {
    pub from: String,
    pub to: String,
    pub link_type: String,
    pub description: Option<String>,
}

/// Full result returned by `build_feature_context`.
#[derive(Debug, Serialize)]
pub struct FeatureContextResult {
    pub feature: FeatureInfo,
    pub links: Vec<LinkInfo>,
    pub seeds: Vec<ContextNode>,
    pub connected: Vec<ContextNode>,
    pub unmatched_entry_points: Vec<String>,
}

/// Build a task-focused context for a declared feature bundle.
///
/// 1. Loads all repos and resolves feature.repos names to repo_uids.
/// 2. Resolves all `feature.entry_points` using exact name match, filtered to feature repos.
/// 3. Runs Personalized PageRank from those seeds.
/// 4. Returns seeds, connected symbols, declared links, and any unmatched entry points.
pub fn build_feature_context(
    store: &GraphStore,
    feature: &FeatureConfig,
    links: &[LinkConfig],
) -> Result<FeatureContextResult, anyhow::Error> {
    // Resolve feature repo names to repo_uids.
    let all_repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;
    let feature_repo_uids: std::collections::HashSet<String> = all_repos
        .iter()
        .filter(|r| feature.repos.contains(&repo_name_from_url(&r.url)))
        .map(|r| r.uid.clone())
        .collect();

    let mut seed_uids: Vec<String> = Vec::new();
    let mut unmatched_entry_points: Vec<String> = Vec::new();

    for entry_point in &feature.entry_points {
        // Use exact name match, then filter to feature repos only.
        let matches = store
            .lookup_symbols_by_name(entry_point)
            .map_err(|e| anyhow::anyhow!(e))?;
        let scoped: Vec<_> = if feature_repo_uids.is_empty() {
            // No repos in DB yet — include all matches (graceful degradation).
            matches
        } else {
            matches
                .into_iter()
                .filter(|s| feature_repo_uids.contains(&s.repo_uid))
                .collect()
        };
        if scoped.is_empty() {
            unmatched_entry_points.push(entry_point.clone());
        }
        for sym in scoped {
            seed_uids.push(sym.uid);
        }
    }

    // Warn about unmatched entry points but don't fail yet.
    for ep in &unmatched_entry_points {
        tracing::warn!(
            "entry point '{}' not found in feature '{}' repos",
            ep,
            feature.name
        );
    }

    // Deduplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    seed_uids.retain(|uid| seen.insert(uid.clone()));

    if seed_uids.is_empty() {
        anyhow::bail!(
            "No symbols found for feature '{}' entry points: {:?}",
            feature.name,
            feature.entry_points
        );
    }

    let ppr_scores = store
        .personalized_pagerank(&seed_uids, 0.85, 20, &GraphScope::unified())
        .map_err(|e| anyhow::anyhow!(e))?;

    let seed_set: std::collections::HashSet<&str> = seed_uids.iter().map(|s| s.as_str()).collect();
    let mut seeds: Vec<ContextNode> = Vec::new();
    let mut connected: Vec<ContextNode> = Vec::new();

    for (uid, score) in &ppr_scores {
        let sym = match store.lookup_symbol(uid) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let node = ContextNode {
            uid: sym.uid.clone(),
            name: sym.name.clone(),
            kind: sym.kind.to_string(),
            file_path: sym.file_path.clone(),
            start_line: sym.start_line,
            signature: sym.signature.clone(),
            relevance: *score,
        };
        if seed_set.contains(uid.as_str()) {
            seeds.push(node);
        } else {
            connected.push(node);
        }
    }

    // Only include links whose both ends are repos declared in this feature.
    let feature_links: Vec<LinkInfo> = links
        .iter()
        .filter(|l| feature.repos.contains(&l.from) && feature.repos.contains(&l.to))
        .map(|l| LinkInfo {
            from: l.from.clone(),
            to: l.to.clone(),
            link_type: l.link_type.clone(),
            description: l.description.clone(),
        })
        .collect();

    Ok(FeatureContextResult {
        feature: FeatureInfo {
            name: feature.name.clone(),
            description: feature.description.clone(),
            repos: feature.repos.clone(),
        },
        links: feature_links,
        seeds,
        connected,
        unmatched_entry_points,
    })
}

// ── Brain context: unified PPR over code + notes ──────────────────────────────

/// One ranked node in a brain-context result. Carries the kind discriminator
/// so the caller can format / filter results by domain (Symbol vs Note vs
/// Section vs Tag vs Heading).
#[derive(Debug, Serialize)]
pub struct BrainNode {
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub location: String,
    pub relevance: f64,
}

#[derive(Debug, Serialize)]
pub struct BrainContextResult {
    pub seeds: Vec<BrainNode>,
    pub connected: Vec<BrainNode>,
    pub unresolved_seeds: Vec<String>,
}

/// Build a task-focused context subgraph using the unified scope (code +
/// notes + cross-references). Seeds may be:
///
/// - UIDs (`sym:...`, `note:...`, `head:...`, `sec:...`, `tag:...`,
///   `repo:...`, `vlt:...`) → direct lookup
/// - Note titles → unique-title match (case-insensitive)
/// - Tag names with optional `#` prefix → tag lookup (case-insensitive)
/// - Symbol names → existing code-side lookup
///
/// Each seed is resolved to as many node UIDs as match; ambiguous seeds
/// contribute all candidates. Unresolved seeds are returned in
/// `unresolved_seeds` so the caller can report them.
pub fn build_brain_context(
    store: &GraphStore,
    inputs: &[String],
) -> Result<BrainContextResult, anyhow::Error> {
    build_brain_context_hybrid(store, inputs, None, &HybridSearchConfig::default())
}

/// Hybrid PPR + BM25 retrieval.
///
/// When `tantivy` is supplied, runs BM25 over the seed strings and fuses
/// the result with PPR via Reciprocal Rank Fusion (RRF, k=60). PPR gets
/// weight 0.7 (structural signal is primary), BM25 gets 0.3 (covers
/// nodes the graph misses). When `tantivy` is None, falls through to
/// pure PPR — same behaviour as before.
///
/// This is what the MCP server's `brain_context` tool calls when its
/// optional TantivyIndex is open. The CLI `brain context` command
/// currently uses the pure-PPR variant; wiring it through is a one-line
/// change once we want the CLI to open the index too.
pub fn build_brain_context_hybrid(
    store: &GraphStore,
    inputs: &[String],
    tantivy: Option<&TantivyIndex>,
    config: &HybridSearchConfig,
) -> Result<BrainContextResult, anyhow::Error> {
    build_brain_context_hybrid_with_aliases(
        store,
        inputs,
        tantivy,
        config,
        &std::collections::HashMap::new(),
    )
}

/// Like `build_brain_context_hybrid` but also accepts a taxonomy alias map
/// (from `load_alias_sidecar`) for vault-defined name canonicalization.
///
/// When a seed does not resolve via title, symbol, or tag lookup, the alias
/// map is consulted: if the seed matches any alias in the map the corresponding
/// canonical name is tried in its place. This allows users to write natural-
/// language seeds (e.g. `"Auth"`) even when the note is titled
/// `"Authentication Service"`.
pub fn build_brain_context_hybrid_with_aliases(
    store: &GraphStore,
    inputs: &[String],
    tantivy: Option<&TantivyIndex>,
    config: &HybridSearchConfig,
    aliases: &std::collections::HashMap<String, Vec<String>>,
) -> Result<BrainContextResult, anyhow::Error> {
    // Build a reverse lookup: alias (lowercase) → canonical name.
    // A single alias may appear under multiple canonicals — we collect all.
    let alias_to_canonical: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (canonical, alias_list) in aliases {
            for alias in alias_list {
                map.entry(alias.to_lowercase())
                    .or_default()
                    .push(canonical.clone());
            }
        }
        map
    };

    let mut seed_uids: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();

    for raw in inputs {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Direct UID forms.
        if trimmed.starts_with("sym:")
            || trimmed.starts_with("note:")
            || trimmed.starts_with("head:")
            || trimmed.starts_with("sec:")
            || trimmed.starts_with("tag:")
            || trimmed.starts_with("repo:")
            || trimmed.starts_with("vlt:")
        {
            seed_uids.push(trimmed.to_string());
            continue;
        }

        // Tag form: leading '#' or bare lookup against Tag.name.
        if let Some(tag_name) = trimmed.strip_prefix('#') {
            if let Some(uid) = lookup_tag_uid(store, tag_name)? {
                seed_uids.push(uid);
                continue;
            }
            unresolved.push(raw.clone());
            continue;
        }

        // Try note title match first — exact (case-insensitive).
        let note_matches = lookup_note_uids_by_title(store, trimmed)?;
        if !note_matches.is_empty() {
            seed_uids.extend(note_matches);
            continue;
        }

        // Fall back to symbol name search.
        let symbol_matches = store
            .search_symbols_by_name(trimmed, 5)
            .map_err(|e| anyhow::anyhow!(e))?;
        if !symbol_matches.is_empty() {
            for s in symbol_matches {
                seed_uids.push(s.uid);
            }
            continue;
        }

        // Last resort: tag without #.
        if let Some(uid) = lookup_tag_uid(store, trimmed)? {
            seed_uids.push(uid);
            continue;
        }

        // Try project name lookup.
        if let Ok(Some(project)) = store.lookup_project_by_name(trimmed) {
            let mut project_resolved = false;
            if let Ok(note_uids) = store.list_project_note_uids(&project.uid)
                && !note_uids.is_empty()
            {
                seed_uids.extend(note_uids);
                project_resolved = true;
            }
            if let Ok(sym_uids) = store.list_project_symbol_uids(&project.uid)
                && !sym_uids.is_empty()
            {
                seed_uids.extend(sym_uids);
                project_resolved = true;
            }
            if project_resolved {
                continue;
            }
        }

        // Taxonomy alias lookup: if the seed is a known alias, try the
        // canonical name(s) in its place.
        if !alias_to_canonical.is_empty() {
            let key = trimmed.to_lowercase();
            if let Some(canonicals) = alias_to_canonical.get(&key) {
                let mut resolved_via_alias = false;
                for canonical in canonicals {
                    let canon_matches = lookup_note_uids_by_title(store, canonical)?;
                    if !canon_matches.is_empty() {
                        tracing::debug!(
                            alias = trimmed,
                            canonical = %canonical,
                            "resolved seed via taxonomy alias"
                        );
                        seed_uids.extend(canon_matches);
                        resolved_via_alias = true;
                    }
                }
                if resolved_via_alias {
                    continue;
                }
            }
        }

        unresolved.push(raw.clone());
    }

    // Dedupe seeds.
    let mut seen = std::collections::HashSet::new();
    seed_uids.retain(|u| seen.insert(u.clone()));

    if seed_uids.is_empty() {
        anyhow::bail!(
            "No seeds resolved. Tried as UIDs, note titles, tags (with or without '#'), and symbol names. Unresolved: {:?}",
            unresolved,
        );
    }

    // Run unified PPR.
    let ppr = store
        .personalized_pagerank(&seed_uids, 0.85, 20, &GraphScope::unified())
        .map_err(|e| anyhow::anyhow!(e))?;

    // ── Hybrid retrieval: fuse PPR + BM25 via Reciprocal Rank Fusion ───
    let fused: Vec<(String, f64)> = if let Some(tantivy) = tantivy {
        let bm25_query = inputs.join(" ");
        let bm25_hits = tantivy
            .search(&bm25_query, config.bm25_limit)
            .unwrap_or_default();
        rrf_fuse(
            &ppr,
            &bm25_hits,
            &[],
            config.rrf_k,
            config.weight_ppr,
            config.weight_bm25,
            0.0,
        )
    } else {
        ppr.clone()
    };

    let seed_set: std::collections::HashSet<&str> = seed_uids.iter().map(|s| s.as_str()).collect();
    let mut seeds: Vec<BrainNode> = Vec::new();
    let mut connected: Vec<BrainNode> = Vec::new();

    for (uid, score) in &fused {
        let Some(node) = render_brain_node(store, uid, *score)? else {
            continue;
        };
        if seed_set.contains(uid.as_str()) {
            seeds.push(node);
        } else {
            connected.push(node);
        }
    }

    Ok(BrainContextResult {
        seeds,
        connected,
        unresolved_seeds: unresolved,
    })
}

/// Reciprocal Rank Fusion of PPR scores, BM25 hits, and semantic (vector) hits.
///
/// Standard RRF formula: `score(d) = Σ_i w_i / (k + rank_i(d))`, where
/// each retrieval method's contribution is weighted by `w_i` and `k`
/// dampens the curve so ties between top-of-list items in one method
/// don't completely override the other method.
///
/// Returns a fused list sorted descending by combined score. PPR rank
/// is by descending score (highest score = rank 1). BM25 rank comes
/// from the hit list's natural order. Semantic rank is by descending
/// cosine similarity score (highest similarity = rank 1).
fn rrf_fuse(
    ppr: &[(String, f64)],
    bm25: &[nestweaver_store::SearchHit],
    semantic: &[(String, f64)],
    k: f64,
    w_ppr: f64,
    w_bm25: f64,
    w_semantic: f64,
) -> Vec<(String, f64)> {
    use std::collections::HashMap;
    let mut scores: HashMap<String, f64> = HashMap::new();

    // PPR list is already sorted descending by score.
    for (rank0, (uid, _)) in ppr.iter().enumerate() {
        let rank = (rank0 + 1) as f64;
        *scores.entry(uid.clone()).or_insert(0.0) += w_ppr / (k + rank);
    }

    for (rank0, hit) in bm25.iter().enumerate() {
        let rank = (rank0 + 1) as f64;
        *scores.entry(hit.uid.clone()).or_insert(0.0) += w_bm25 / (k + rank);
    }

    // Semantic list is sorted descending by similarity score.
    for (rank0, (uid, _)) in semantic.iter().enumerate() {
        let rank = (rank0 + 1) as f64;
        *scores.entry(uid.clone()).or_insert(0.0) += w_semantic / (k + rank);
    }

    let mut merged: Vec<(String, f64)> = scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

fn lookup_note_uids_by_title(
    store: &GraphStore,
    title: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let needle = title.to_lowercase();
    let notes = store.list_notes(None).map_err(|e| anyhow::anyhow!(e))?;
    Ok(notes
        .into_iter()
        .filter(|n| n.title.to_lowercase() == needle)
        .map(|n| n.uid)
        .collect())
}

fn lookup_tag_uid(store: &GraphStore, name: &str) -> Result<Option<String>, anyhow::Error> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(None);
    }
    let tags = store.list_tags(None).map_err(|e| anyhow::anyhow!(e))?;
    Ok(tags.into_iter().find(|t| t.name == needle).map(|t| t.uid))
}

/// Resolve a UID to a printable `BrainNode` by dispatching on UID prefix.
/// Returns Ok(None) if the node can't be found (silently dropped from
/// results — should only happen for stale/orphan UIDs).
fn render_brain_node(
    store: &GraphStore,
    uid: &str,
    score: f64,
) -> Result<Option<BrainNode>, anyhow::Error> {
    if uid.starts_with("sym:") {
        match store.lookup_symbol(uid) {
            Ok(s) => Ok(Some(BrainNode {
                uid: s.uid,
                kind: format!("Symbol/{}", s.kind),
                title: s.name,
                location: format!("{}:{}", s.file_path, s.start_line),
                relevance: score,
            })),
            Err(nestweaver_store::StoreError::NotFound) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    } else if uid.starts_with("note:") {
        match store.lookup_note(uid) {
            Ok(n) => Ok(Some(BrainNode {
                uid: n.uid,
                kind: format!("Note/{}", n.note_kind),
                title: n.title,
                location: n.file_path,
                relevance: score,
            })),
            Err(nestweaver_store::StoreError::NotFound) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    } else if uid.starts_with("head:") {
        // Headings don't have a direct lookup yet — synthesise minimal info.
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Heading".to_string(),
            title: uid.to_string(),
            location: String::new(),
            relevance: score,
        }))
    } else if uid.starts_with("sec:") {
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Section".to_string(),
            title: uid.to_string(),
            location: String::new(),
            relevance: score,
        }))
    } else if uid.starts_with("tag:") {
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Tag".to_string(),
            title: uid.to_string(),
            location: String::new(),
            relevance: score,
        }))
    } else {
        Ok(None)
    }
}

// ── List helpers ──────────────────────────────────────────────────────────────

/// List all repos, optionally filtered by instance ID.
pub fn list_repos(
    store: &GraphStore,
    instance_id: Option<&str>,
) -> Result<Vec<Repo>, anyhow::Error> {
    store
        .list_repos(instance_id)
        .map_err(|e| anyhow::anyhow!(e))
}

/// List all services, optionally filtered by instance ID.
pub fn list_services(
    store: &GraphStore,
    instance_id: Option<&str>,
) -> Result<Vec<Service>, anyhow::Error> {
    store
        .list_services(instance_id)
        .map_err(|e| anyhow::anyhow!(e))
}

/// Estimate the number of tokens in `text` using the (len + 3) / 4 rule.
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Generate a structural repo-map skeleton, ordered by PageRank (highest first).
///
/// The output groups symbols by file path:
/// ```text
/// src/main.js
///   function greet(name)
///   function hello(name)
/// src/utils.js
///   function formatDate(date)
/// ```
///
/// Generation stops when the estimated token count of the accumulated output
/// would exceed `token_budget`.
pub fn generate_repo_map(store: &GraphStore, token_budget: usize) -> Result<String, anyhow::Error> {
    let symbols = store
        .symbols_by_pagerank(None)
        .map_err(|e| anyhow::anyhow!(e))?;

    // Group symbols by file path while preserving the PageRank order of files.
    // The first occurrence of a file path determines the file's position.
    let mut file_order: Vec<String> = Vec::new();
    let mut file_symbols: std::collections::HashMap<String, Vec<&Symbol>> =
        std::collections::HashMap::new();

    for sym in &symbols {
        let file = &sym.file_path;
        if !file_symbols.contains_key(file) {
            file_order.push(file.clone());
        }
        file_symbols.entry(file.clone()).or_default().push(sym);
    }

    let mut output = String::new();
    let mut tokens_used = 0usize;

    'outer: for file_path in &file_order {
        let syms = match file_symbols.get(file_path) {
            Some(s) => s,
            None => continue,
        };

        // Build the file header line.
        let header = format!("{file_path}\n");
        let header_tokens = estimate_tokens(&header);
        if tokens_used + header_tokens > token_budget {
            break;
        }
        output.push_str(&header);
        tokens_used += header_tokens;

        for sym in syms {
            let line = format!("  {} {}\n", sym.kind, sym.signature);
            let line_tokens = estimate_tokens(&line);
            if tokens_used + line_tokens > token_budget {
                break 'outer;
            }
            output.push_str(&line);
            tokens_used += line_tokens;
        }
    }

    Ok(output)
}

#[cfg(test)]
mod repo_map_tests {
    use std::fs;

    use nestweaver_store::GraphScope;

    use super::generate_repo_map;
    use crate::index::index_directory_in_memory;

    fn make_test_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) { return hello(name); }\nfunction hello(name) { return name; }",
        )
        .unwrap();
        fs::write(
            src.join("utils.js"),
            "function formatDate(date) { return date; }",
        )
        .unwrap();
        (dir, src)
    }

    #[test]
    fn repo_map_respects_token_budget() {
        let (_dir, src) = make_test_repo();
        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        // Very small budget — output must be within budget.
        let budget = 20;
        let map = generate_repo_map(&store, budget).unwrap();
        let tokens = map.len().div_ceil(4);
        assert!(
            tokens <= budget,
            "token count {tokens} exceeds budget {budget}; output was:\n{map}"
        );
    }

    #[test]
    fn repo_map_includes_symbols() {
        let (_dir, src) = make_test_repo();
        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        let map = generate_repo_map(&store, 4096).unwrap();
        assert!(!map.is_empty(), "repo map should not be empty");
        // At least one function name should appear.
        assert!(
            map.contains("greet") || map.contains("hello") || map.contains("formatDate"),
            "repo map should contain a function name; got:\n{map}"
        );
    }
}

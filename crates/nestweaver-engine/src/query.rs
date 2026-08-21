use nestweaver_schema::{Repo, Service, Symbol};
use nestweaver_store::{GraphScope, GraphStore, QueryIntent, TantivyIndex, detect_intent};
use serde::{Deserialize, Serialize};

use anyhow::Context;

use crate::config::{
    FeatureConfig, LinkConfig, RANKING_MULTIPLIER_MAX, RANKING_MULTIPLIER_MIN, RankingConfig,
    RepoConfig,
};
use crate::repo_display_name;

// ── Embed abstraction ─────────────────────────────────────────────────────
/// Trait abstracting over an embedding model's query-embedding capability.
///
/// This is feature-gated behind `embed` so that callers without the
/// `nestweaver-embed` crate can still compile: they simply pass
/// `None::<&dyn EmbedQueryFn>` at each call site.
pub trait EmbedQueryFn: Send + Sync {
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    fn pipeline_for_dimension(
        &self,
        dimension: usize,
    ) -> anyhow::Result<nestweaver_schema::EmbeddingPipelineV2> {
        Ok(nestweaver_schema::EmbeddingPipelineV2::external(
            "opaque-runtime",
            "unknown-test-or-custom-model",
            u32::try_from(dimension)?,
        ))
    }
}

/// Supplies the current embedding model from a runtime-owned readiness
/// snapshot. Daemon transports use this indirection so every handler observes
/// the model published atomically with its readiness state.
pub trait EmbedModelProvider: Send + Sync {
    fn current_model(&self) -> Option<std::sync::Arc<dyn EmbedQueryFn>>;
}

#[cfg(feature = "embed")]
impl EmbedQueryFn for nestweaver_embed::EmbedModel {
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        nestweaver_embed::EmbedModel::embed_query(self, text)
    }

    fn pipeline_for_dimension(
        &self,
        dimension: usize,
    ) -> anyhow::Result<nestweaver_schema::EmbeddingPipelineV2> {
        nestweaver_embed::EmbedModel::pipeline_for_dimension(self, dimension)
    }
}

/// Tuning knobs for hybrid PPR + BM25 + semantic retrieval.
///
/// Defaults reflect empirical tuning for weighted score fusion
/// (convex combination with tanh normalization):
/// - `weight_ppr = 0.40` — structural signal from Personalized PageRank.
/// - `weight_bm25 = 0.25` — lexical relevance covers what graph
///   structure misses (freshly-mentioned terms, exact matches).
/// - `weight_semantic = 0.35` — embedding-based similarity. Effectively
///   0.0 on databases without generated embeddings (`nestweaver embed`):
///   the semantic leg is skipped entirely when the store holds no
///   embedding vectors, so no BERT forward pass is paid.
/// - `bm25_limit = 500` — large enough that BM25's tail is comparable
///   to PPR's. 500 keeps the candidate pool symmetric.
/// - `semantic_limit = 200` — cap on vector KNN results fed into
///   fusion. Bounded to keep the in-process cosine scan tractable.
#[derive(Debug, Clone)]
pub struct HybridSearchConfig {
    pub weight_ppr: f64,
    pub weight_bm25: f64,
    pub weight_semantic: f64,
    pub bm25_limit: usize,
    pub semantic_limit: usize,
    /// Feature F7 (PRF half) — when `true`, the BM25 leg of hybrid retrieval
    /// runs a two-pass pseudo-relevance-feedback expansion before fusion.
    /// Off by default → identical behaviour to before.
    ///
    /// PRF NOTE: with weighted score fusion the PRF expansion weight
    /// ([`nestweaver_store::PRF_EXPANSION_WEIGHT`] = 0.3) affects the raw BM25
    /// scores that flow into tanh normalization. The expansion terms boost
    /// matching documents' BM25 scores, which then carry through fusion.
    pub prf: bool,
    /// Finding #7 — graduated path-deboost + kind-priority for
    /// `search_symbols_by_name` seed resolution. Sourced from
    /// `[seed_resolution]` in instance config (with backward-compat shim
    /// for the legacy `[ranking].test_path_patterns` block).
    pub seed_resolution: nestweaver_store::SeedResolutionConfig,
    /// When `true`, semantic scores are always mixed in even when the query
    /// matched zero BM25 results. Sourced from `[embedding].always_blend_semantic`.
    pub always_blend_semantic: bool,
    /// Maximum number of top semantic hits injected as PPR seeds.
    /// Sourced from `[embedding].semantic_seed_limit`.
    pub semantic_seed_limit: usize,
}

impl Default for HybridSearchConfig {
    fn default() -> Self {
        Self {
            weight_ppr: 0.40,
            weight_bm25: 0.25,
            weight_semantic: 0.35,
            bm25_limit: 500,
            semantic_limit: 200,
            prf: false,
            seed_resolution: nestweaver_store::SeedResolutionConfig::default(),
            always_blend_semantic: true,
            semantic_seed_limit: 5,
        }
    }
}

/// Full details for a single symbol, including its call graph neighbours.
#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolDetail {
    pub symbol: Symbol,
    pub callers: Vec<Symbol>,
    pub callees: Vec<Symbol>,
}

/// A lightweight summary used for disambiguation and search results.
#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolCandidate {
    pub uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
}

/// Engine-facing counted symbol candidates. The total relation is preserved
/// verbatim from the bounded store search.
#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolCandidatePage {
    pub results: Vec<SymbolCandidate>,
    pub total: nestweaver_store::tantivy_index::SearchTotal,
}

impl From<&Symbol> for SymbolCandidate {
    fn from(s: &Symbol) -> Self {
        SymbolCandidate {
            uid: s.uid.clone(),
            canonical_id: s.canonical_id.clone(),
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
pub fn lookup_symbol(
    store: &GraphStore,
    name_or_uid: &str,
    instance: Option<&str>,
) -> Result<LookupResult, anyhow::Error> {
    // When an instance filter is requested, validate it up front — an unknown instance is a
    // user error (not an empty result) — and build a repo_uid -> instance_id map to scope
    // matches. Symbols carry their instance via their repo.
    let repo_instance = match instance {
        Some(inst) => {
            let repos = store
                .list_repos(None)
                .context("list repos for --instance filter")?;
            let known: std::collections::HashSet<&str> =
                repos.iter().map(|r| r.instance_id.as_str()).collect();
            if !known.contains(inst) {
                let mut valid: Vec<&str> = known.iter().copied().collect();
                valid.sort_unstable();
                // Does `inst` name a project/vault instance? Then it's the nw-019
                // three-conventions mismatch — repos hash the db path, projects use
                // the logical config name, the vault uses `default`. Point the user
                // at the consolidation fix instead of dead-ending.
                let project_vault_instances: std::collections::HashSet<String> = store
                    .list_projects()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.instance_id.clone())
                    .chain(
                        store
                            .list_vaults(None)
                            .unwrap_or_default()
                            .iter()
                            .map(|v| v.instance_id.clone()),
                    )
                    .collect();
                if project_vault_instances.contains(inst) {
                    let repo_inst = valid.first().copied().unwrap_or("<repo-instance>");
                    anyhow::bail!(
                        "instance '{inst}' names projects or vaults but no repos; repos in \
                         this DB are indexed under: {}. To consolidate, run `nestweaver \
                         instance merge --from {repo_inst} --to {inst}` and then force \
                         re-index each repo.",
                        valid.join(", ")
                    );
                }
                anyhow::bail!(
                    "unknown instance '{inst}'; valid instances: {}",
                    valid.join(", ")
                );
            }
            Some(
                repos
                    .into_iter()
                    .map(|r| (r.uid, r.instance_id))
                    .collect::<std::collections::HashMap<String, String>>(),
            )
        }
        None => None,
    };
    let in_instance = |repo_uid: &str| -> bool {
        match (&repo_instance, instance) {
            (Some(map), Some(inst)) => map.get(repo_uid).map(|i| i == inst).unwrap_or(false),
            _ => true,
        }
    };

    if name_or_uid.contains(':') {
        // UID path
        match store.lookup_symbol(name_or_uid) {
            Ok(sym) => {
                // A UID in another instance is "not found" under this filter.
                if !in_instance(&sym.repo_uid) {
                    return Ok(LookupResult::NotFound);
                }
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
        let matches: Vec<_> = store
            .lookup_symbols_by_name(name_or_uid)
            .context("name lookup")?
            .into_iter()
            .filter(|s| in_instance(&s.repo_uid))
            .collect();

        match matches.len() {
            0 => Ok(LookupResult::NotFound),
            1 => {
                let sym = matches.into_iter().next().expect("checked len == 1");
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
    Ok(search_symbols_page(store, query, limit)?.results)
}

/// Search for a bounded page of ranked symbols and independent total metadata.
pub fn search_symbols_page(
    store: &GraphStore,
    query: &str,
    limit: usize,
) -> Result<SymbolCandidatePage, anyhow::Error> {
    let page = store
        .search_symbols_by_name_page(
            query,
            limit,
            &nestweaver_store::SeedResolutionConfig::default(),
        )
        .context("search_symbols_by_name")?;
    Ok(SymbolCandidatePage {
        results: page.symbols.iter().map(SymbolCandidate::from).collect(),
        total: page.total,
    })
}

#[cfg(test)]
mod counted_symbol_search_tests {
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};
    use nestweaver_store::{GraphStore, TantivyIndex, tantivy_index::SearchTotalRelation};

    use super::{search_symbols, search_symbols_page};

    fn symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: format!("src/{uid}.rs"),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: Some(format!("canonical:{uid}")),
        }
    }

    #[test]
    fn search_symbols_page_preserves_ranked_results_and_count_metadata() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&symbol("s1", "Payment")).unwrap();
        store
            .insert_symbol(&symbol("s2", "PaymentGateway"))
            .unwrap();

        let page = search_symbols_page(&store, "payment", 1).unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.total.value, 2);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
        assert_eq!(
            page.results[0].canonical_id.as_deref(),
            Some("canonical:s1")
        );

        let legacy = search_symbols(&store, "payment", 1).unwrap();
        assert_eq!(page.results[0].uid, legacy[0].uid);
    }

    #[test]
    fn engine_symbol_search_rejects_extreme_presentation_limits() {
        use nestweaver_store::SEARCH_PRESENTATION_LIMIT_MAX;

        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&symbol("s1", "Payment")).unwrap();

        assert!(search_symbols(&store, "payment", 0).unwrap().is_empty());
        assert!(
            search_symbols_page(&store, "payment", 0)
                .unwrap()
                .results
                .is_empty()
        );
        assert!(search_symbols(&store, "payment", SEARCH_PRESENTATION_LIMIT_MAX).is_ok());
        assert!(search_symbols_page(&store, "payment", SEARCH_PRESENTATION_LIMIT_MAX).is_ok());
        for over_limit in [SEARCH_PRESENTATION_LIMIT_MAX + 1, usize::MAX] {
            assert!(search_symbols(&store, "payment", over_limit).is_err());
            assert!(search_symbols_page(&store, "payment", over_limit).is_err());
        }
    }

    #[test]
    fn search_symbols_keeps_same_title_note_and_code_symbol_in_distinct_domains() {
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index
            .update_note(
                "note:payment",
                "Payment",
                "vlt:t",
                &["payment".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let note_page = index.search_page("payment", 1).unwrap();

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("sym:payment", "Payment"))
            .unwrap();
        let symbol_page = search_symbols_page(&store, "payment", 1).unwrap();

        assert_eq!(note_page.total.value, 1);
        assert_eq!(symbol_page.total.value, 1);
        assert_eq!(note_page.total.value + symbol_page.total.value, 2);
    }
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
///
/// When `intent` is `None`, defaults to the standard damping (0.85) with
/// no edge weight adjustments, preserving backward compatibility.
pub fn build_context(
    store: &GraphStore,
    inputs: &[String],
) -> Result<ContextResult, anyhow::Error> {
    build_context_with_intent(store, inputs, None, None)
}

/// Like [`build_context`] but accepts an optional [`QueryIntent`] to
/// dynamically tune PPR's damping factor and edge weights.
///
/// When `intent` is `Some`, the intent's parameters override the defaults.
/// When `intent` is `None` but auto-detection is desired, use
/// [`QueryIntent`] with [`detect_intent`] on the resolved seeds.
pub fn build_context_with_intent(
    store: &GraphStore,
    inputs: &[String],
    intent: Option<QueryIntent>,
    limit: Option<usize>,
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
                .search_symbols_by_name(
                    input,
                    5,
                    &nestweaver_store::SeedResolutionConfig::default(),
                )
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

    // Resolve the effective intent: use the caller's override if provided,
    // otherwise auto-detect from the resolved seeds.
    let effective_intent = intent.or_else(|| Some(detect_intent(store, &seed_uids)));

    // Run Personalized PageRank over the code-only scope (preserves the
    // pre-brain behaviour of `nestweaver context`). The unified scope that
    // mixes code + notes is exposed via `nestweaver brain context`.
    let ppr_results = store
        .personalized_pagerank_with_intent(
            &seed_uids,
            0.85,
            20,
            &GraphScope::code_only(),
            effective_intent,
        )
        .map_err(|e| anyhow::anyhow!(e))?;

    let seed_set: std::collections::HashSet<&str> = seed_uids.iter().map(|s| s.as_str()).collect();

    let mut seeds: Vec<ContextNode> = Vec::new();
    let mut connected: Vec<ContextNode> = Vec::new();
    let effective_limit = limit.unwrap_or(usize::MAX);

    // Batch-fetch all PPR-ranked symbols in a single query to avoid N+1 overhead.
    let ppr_uids: Vec<&str> = ppr_results.iter().map(|(u, _)| u.as_str()).collect();
    let sym_map = store
        .batch_lookup_symbols(&ppr_uids)
        .map_err(|e| anyhow::anyhow!(e))?;

    for (uid, score) in &ppr_results {
        let sym = match sym_map.get(uid.as_str()) {
            Some(s) => s,
            None => continue,
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
        } else if connected.len() < effective_limit {
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
mod promote_tests {
    use super::{
        BrainContextResult, BrainNode, promote_member_notes_into_connected,
        promote_member_symbols_into_connected,
    };
    use std::collections::HashSet;

    fn note(uid: &str) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: "Note".to_string(),
            title: uid.to_string(),
            location: format!("Projects/x/{uid}.md"),
            relevance: 0.9,
            inline_body: None,
            body_complete: true,
        }
    }

    fn symbol(uid: &str) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: "Symbol".to_string(),
            title: uid.to_string(),
            location: format!("src/{uid}.rs"),
            relevance: 0.5,
            inline_body: None,
            body_complete: true,
        }
    }

    // Bug #12: when a project declares repos, its member notes are seeded into
    // PPR (to survive the min_score filter) and therefore land in `seeds`,
    // which is disjoint from `connected` and never rendered. The notes must be
    // surfaced into `connected` so project orientation actually shows them.
    #[test]
    fn promotes_member_notes_from_seeds_into_connected() {
        let mut result = BrainContextResult {
            seeds: vec![note("note:prd"), note("note:status")],
            connected: vec![symbol("sym:handler")],
            unresolved_seeds: vec![],
            expansion_terms: vec![],
            semantic_applied: false,
            semantic_seed_count: 0,
            degraded_components: vec![],
        };
        let members: HashSet<String> = ["note:prd".to_string(), "note:status".to_string()]
            .into_iter()
            .collect();

        promote_member_notes_into_connected(&mut result, &members);

        let connected_uids: Vec<&str> = result.connected.iter().map(|n| n.uid.as_str()).collect();
        assert!(
            connected_uids.contains(&"note:prd"),
            "member note 'note:prd' must surface in connected; got {connected_uids:?}"
        );
        assert!(
            connected_uids.contains(&"note:status"),
            "member note 'note:status' must surface in connected; got {connected_uids:?}"
        );
    }

    // Non-member seeds (e.g. the project node itself) stay out of connected,
    // and a member note already present is not duplicated.
    #[test]
    fn does_not_duplicate_or_promote_non_members() {
        let mut result = BrainContextResult {
            seeds: vec![note("note:prd"), symbol("proj:x")],
            connected: vec![note("note:prd"), symbol("sym:handler")],
            unresolved_seeds: vec![],
            expansion_terms: vec![],
            semantic_applied: false,
            semantic_seed_count: 0,
            degraded_components: vec![],
        };
        let members: HashSet<String> = ["note:prd".to_string()].into_iter().collect();

        promote_member_notes_into_connected(&mut result, &members);

        let prd_count = result
            .connected
            .iter()
            .filter(|n| n.uid == "note:prd")
            .count();
        assert_eq!(
            prd_count, 1,
            "already-present member note must not be duplicated"
        );
        assert!(
            !result.connected.iter().any(|n| n.uid == "proj:x"),
            "non-member seed must not be promoted"
        );
    }

    // Wave-5 regression fix: when a project declares repos, the project node's
    // mass fans out across thousands of PROJECT_INCLUDES_SYMBOL edges, leaving
    // each member symbol below the PPR min_score filter. The CLI seeds the
    // top-K member symbols by PageRank to keep them alive, and this helper
    // surfaces them into `connected` (mirror of the notes promotion).
    #[test]
    fn promotes_member_symbols_from_seeds_into_connected() {
        let mut result = BrainContextResult {
            seeds: vec![
                symbol("sym:foo"),
                symbol("sym:bar"),
                note("note:non-member"),
            ],
            connected: vec![note("note:overview")],
            unresolved_seeds: vec![],
            expansion_terms: vec![],
            semantic_applied: false,
            semantic_seed_count: 0,
            degraded_components: vec![],
        };
        let members: HashSet<String> = ["sym:foo".to_string(), "sym:bar".to_string()]
            .into_iter()
            .collect();

        promote_member_symbols_into_connected(&mut result, &members);

        let connected_uids: Vec<&str> = result.connected.iter().map(|n| n.uid.as_str()).collect();
        assert!(
            connected_uids.contains(&"sym:foo"),
            "member symbol 'sym:foo' must surface in connected; got {connected_uids:?}"
        );
        assert!(
            connected_uids.contains(&"sym:bar"),
            "member symbol 'sym:bar' must surface in connected; got {connected_uids:?}"
        );
        assert!(
            !connected_uids.contains(&"note:non-member"),
            "non-member seed must not be promoted; got {connected_uids:?}"
        );
    }

    #[test]
    fn symbol_promotion_does_not_duplicate_already_connected() {
        let mut result = BrainContextResult {
            seeds: vec![symbol("sym:foo")],
            connected: vec![symbol("sym:foo")],
            unresolved_seeds: vec![],
            expansion_terms: vec![],
            semantic_applied: false,
            semantic_seed_count: 0,
            degraded_components: vec![],
        };
        let members: HashSet<String> = ["sym:foo".to_string()].into_iter().collect();

        promote_member_symbols_into_connected(&mut result, &members);

        let count = result
            .connected
            .iter()
            .filter(|n| n.uid == "sym:foo")
            .count();
        assert_eq!(count, 1, "already-present member symbol must not duplicate");
    }
}

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
        let search = store
            .search_symbols_by_name(
                "formatDate",
                1,
                &nestweaver_store::SeedResolutionConfig::default(),
            )
            .unwrap();
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
/// 1. Loads all repos and resolves feature.repos names to repo_uids. Matching
///    accepts either the DB Repo display name or an `[[repos]]` config alias
///    (`name = "..."`) whose URL matches an indexed repo — so a feature can
///    refer to a repo by a friendly name even if it was indexed under its
///    URL basename.
/// 2. Resolves all `feature.entry_points` using exact name match, filtered to feature repos.
/// 3. Runs Personalized PageRank from those seeds.
/// 4. Returns seeds, connected symbols, declared links, and any unmatched entry points.
pub fn build_feature_context(
    store: &GraphStore,
    feature: &FeatureConfig,
    links: &[LinkConfig],
    repo_configs: &[RepoConfig],
    intent: Option<QueryIntent>,
    limit: Option<usize>,
) -> Result<FeatureContextResult, anyhow::Error> {
    // Resolve feature repo names to repo_uids.
    let all_repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;
    // URL allow-list derived from `[[repos]] name = ...` aliases declared by
    // the feature. Lets `feature.repos = ["redrock"]` resolve to a DB repo
    // indexed under a different display name when the config aliases it.
    let alias_urls: std::collections::HashSet<&str> = repo_configs
        .iter()
        .filter(|rc| {
            rc.name
                .as_deref()
                .is_some_and(|n| feature.repos.iter().any(|fr| fr == n))
        })
        .map(|rc| rc.url.as_str())
        .collect();
    let feature_repo_uids: std::collections::HashSet<String> = all_repos
        .iter()
        .filter(|r| {
            feature.repos.contains(&repo_display_name(r)) || alias_urls.contains(r.url.as_str())
        })
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
        .personalized_pagerank_with_intent(&seed_uids, 0.85, 20, &GraphScope::unified(), intent)
        .map_err(|e| anyhow::anyhow!(e))?;

    let seed_set: std::collections::HashSet<&str> = seed_uids.iter().map(|s| s.as_str()).collect();
    let mut seeds: Vec<ContextNode> = Vec::new();
    let mut connected: Vec<ContextNode> = Vec::new();

    // Apply limit to PPR results (seeds are always included).
    let effective_limit = limit.unwrap_or(usize::MAX);
    let ppr_uids: Vec<&str> = ppr_scores.iter().map(|(u, _)| u.as_str()).collect();
    let sym_map = store
        .batch_lookup_symbols(&ppr_uids)
        .map_err(|e| anyhow::anyhow!(e))?;

    for (uid, score) in &ppr_scores {
        let sym = match sym_map.get(uid.as_str()) {
            Some(s) => s,
            None => continue,
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
        } else if connected.len() < effective_limit {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainNode {
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub location: String,
    pub relevance: f64,
    /// Feature F8 (tiered inline bodies): the node's source body, populated
    /// only when the caller opted in *and* the node's normalized relevance
    /// cleared the configured threshold. `None` (and omitted from JSON) by
    /// default so existing callers see unchanged output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_body: Option<String>,
    /// Bug H — fidelity signal for `inline_body`. `true` when the inlined
    /// body contains the full source, `false` when the per-body cap forced
    /// truncation. Skipped from JSON when `true` so existing consumers see
    /// unchanged output and only learn about the field when it flags a
    /// truncated body.
    ///
    /// `#[serde(default = "default_body_complete")]` is required because the
    /// daemon's JSON-RPC `GetContext` response routes through
    /// `serde_json::from_str` on the client. When a node serializes with
    /// `body_complete=true` the field is omitted (per the `skip_serializing_if`
    /// above); without the default, the client deserialization fails with
    /// `missing field body_complete`, causing daemon-routed `brain context` to
    /// hard-error. The default mirrors constructor behavior (no truncation =>
    /// complete).
    #[serde(skip_serializing_if = "is_true", default = "default_body_complete")]
    pub body_complete: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

/// Serde default for [`BrainNode::body_complete`]. See the field doc on
/// `BrainNode::body_complete` for the daemon-deserialization rationale.
fn default_body_complete() -> bool {
    true
}

/// Char-truncate `body` to `max_chars`, preferring the last newline within the
/// truncated range so we never split a statement mid-line. Returns the (possibly
/// shortened) body plus a `body_complete` flag — `true` when no truncation was
/// needed. Safe on UTF-8: the char-iter cap respects codepoint boundaries.
pub(crate) fn truncate_body_to_chars(body: String, max_chars: usize) -> (String, bool) {
    if body.chars().count() <= max_chars {
        return (body, true);
    }
    let mut truncated: String = body.chars().take(max_chars).collect();
    if let Some(last_nl) = truncated.rfind('\n') {
        truncated.truncate(last_nl);
    }
    (truncated, false)
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BrainContextResult {
    /// Resolved seed nodes. The MCP `brain_context` tool omits this field
    /// when `include_seeds=false` is requested, so deserializing daemon
    /// `GetContext` responses requires a default.
    #[serde(default)]
    pub seeds: Vec<BrainNode>,
    pub connected: Vec<BrainNode>,
    /// Seed strings that did not resolve to any UID. The MCP
    /// `brain_context` tool omits this field when empty, so deserializing
    /// daemon `GetContext` responses requires a default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_seeds: Vec<String>,
    /// Feature F7 (PRF half) — terms mined by pseudo-relevance feedback and
    /// fed into the pass-2 BM25 query. Empty unless PRF was enabled. Surfaced
    /// for `--debug` / response auditing. Omitted from JSON when empty so the
    /// default (PRF-off) output is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expansion_terms: Vec<String>,
    /// Whether the requested semantic leg embedded the query and completed
    /// vector search successfully.
    #[serde(default)]
    pub semantic_applied: bool,
    /// How many entries in `seeds` were injected by the semantic leg rather
    /// than resolved from the query text.
    ///
    /// nw-102: vector KNN always returns its k nearest neighbours regardless of
    /// how far away they are, and those hits were pushed into `seeds`
    /// unconditionally — the similarity score was discarded at the call site.
    /// So a nonsense query reported "Seeds (5 resolved)" listing unrelated
    /// symbols WHILE also listing the same query under `unresolved_seeds`,
    /// contradicting itself. Recording the provenance lets a renderer say which
    /// seeds were actually matched and which are nearest-neighbour guesses.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub semantic_seed_count: usize,
    /// Retrieval components that were requested but unavailable. The graph,
    /// PPR, and BM25 legs remain usable when semantic retrieval degrades.
    #[serde(default)]
    pub degraded_components: Vec<String>,
}

/// Surface a project's curated member notes into the rendered `connected`
/// list.
///
/// `project_context` seeds PPR from the project's member notes so they (a)
/// survive the `min_score` filter in `personalized_pagerank` — which would
/// otherwise drop them once the project node fans out across tens of
/// thousands of `PROJECT_INCLUDES_SYMBOL` edges — and (b) let the walk
/// explore their neighbourhoods. But seeded UIDs land in `seeds`, which is
/// disjoint from `connected` and is *not* rendered by the CLI / MCP project
/// responses. For project orientation the curated notes are the
/// authoritative answer, so promote any member note that resolved as a seed
/// (and isn't already present) into `connected`. De-duplicated by UID.
pub fn promote_member_notes_into_connected(
    result: &mut BrainContextResult,
    member_note_uids: &std::collections::HashSet<String>,
) {
    let present: std::collections::HashSet<String> =
        result.connected.iter().map(|n| n.uid.clone()).collect();
    let promoted: Vec<BrainNode> = result
        .seeds
        .iter()
        .filter(|n| member_note_uids.contains(&n.uid) && !present.contains(&n.uid))
        .cloned()
        .collect();
    result.connected.extend(promoted);
}

/// Surface a project's curated member symbols into the rendered `connected`
/// list.
///
/// Companion to [`promote_member_notes_into_connected`]. When `project-context`
/// seeds PPR with the top-K project symbols by PageRank, those seeds survive
/// the `min_score` filter but land in `result.seeds` — which is *not* rendered
/// by the CLI / MCP project responses. Promote any seeded symbol UID present
/// in `member_symbol_uids` into `connected`, de-duplicated by UID, so the
/// architecturally important code surfaces alongside the curated notes.
pub fn promote_member_symbols_into_connected(
    result: &mut BrainContextResult,
    member_symbol_uids: &std::collections::HashSet<String>,
) {
    let present: std::collections::HashSet<String> =
        result.connected.iter().map(|n| n.uid.clone()).collect();
    let promoted: Vec<BrainNode> = result
        .seeds
        .iter()
        .filter(|n| member_symbol_uids.contains(&n.uid) && !present.contains(&n.uid))
        .cloned()
        .collect();
    result.connected.extend(promoted);
}

/// Drop `Heading` nodes from `connected` when a `Section` node sharing the
/// same `(file, title)` is already present.
///
/// The vault graph emits both a `Heading` node (the heading line itself) and
/// a `Section` node (the heading plus its body) for every markdown heading.
/// They land in retrieval results with near-identical PPR scores and identical
/// titles — the Section strictly dominates because it carries the body text.
/// In notes-heavy projects this overlap consumes ~25% of a 2000-token budget
/// on duplicate entries that add no information.
///
/// Location normalisation: a Heading's `location` is typically `<file>` or
/// `<file>:<line>` and a Section's is `<file>` or `<file>#<anchor>`; both
/// collapse to the same file stem so the pair is detected regardless of
/// whether anchors / line suffixes are present.
pub fn dedup_heading_section_pairs(result: &mut BrainContextResult) {
    fn loc_stem(loc: &str) -> &str {
        let no_anchor = loc.split_once('#').map(|(p, _)| p).unwrap_or(loc);
        if let Some((p, tail)) = no_anchor.rsplit_once(':')
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            return p;
        }
        no_anchor
    }

    let section_keys: std::collections::HashSet<(String, String)> = result
        .connected
        .iter()
        .filter(|n| n.kind.eq_ignore_ascii_case("Section"))
        .map(|n| (loc_stem(&n.location).to_string(), n.title.clone()))
        .collect();

    if section_keys.is_empty() {
        return;
    }

    result.connected.retain(|n| {
        if !n.kind.eq_ignore_ascii_case("Heading") {
            return true;
        }
        let key = (loc_stem(&n.location).to_string(), n.title.clone());
        !section_keys.contains(&key)
    });
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
    build_brain_context_hybrid(
        store,
        inputs,
        None,
        &HybridSearchConfig::default(),
        None,
        None,
    )
}

/// Whether the store's embedding sidecar index holds any vectors.
///
/// The semantic leg embeds the query text with a full BERT forward pass
/// whenever `weight_semantic > 0` — pure waste on databases where
/// `nestweaver embed` never ran, since there are no vectors to match
/// against. The determination is a per-store property, so it is cached
/// keyed by db path. A positive probe is invalidated whenever
/// `graph_generation` changes; a negative probe is additionally
/// time-bounded (30s) because the embed path adds vectors without
/// advancing the generation, so embeddings added while the process is
/// live (e.g. `nestweaver embed` against a running daemon) are picked up
/// within seconds rather than after an unrelated mutation or a restart.
/// In-memory stores (no db path) probe directly — `embedding_count` is a
/// mutex-guarded `len()`.
fn store_has_embeddings(store: &GraphStore) -> bool {
    let Some(db_path) = store.db_path().map(std::path::Path::to_path_buf) else {
        return store.embedding_count() > 0;
    };
    let generation = store.graph_generation();
    struct Entry {
        generation: u64,
        has: bool,
        probed_at: std::time::Instant,
    }
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Entry>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    match cache.get(&db_path) {
        // A POSITIVE probe is generation-keyed: embeddings are only added,
        // and a graph mutation re-probes.
        Some(entry) if entry.has && entry.generation == generation => true,
        // A NEGATIVE probe is only trusted briefly: the daemon's embed path
        // adds and flushes vectors WITHOUT advancing the graph generation,
        // so a cached "no embeddings" would otherwise hide live embeddings
        // until an unrelated mutation or a restart.
        Some(entry)
            if !entry.has
                && entry.generation == generation
                && entry.probed_at.elapsed() < std::time::Duration::from_secs(30) =>
        {
            false
        }
        _ => {
            let has = store.embedding_count() > 0;
            cache.insert(
                db_path,
                Entry {
                    generation,
                    has,
                    probed_at: std::time::Instant::now(),
                },
            );
            has
        }
    }
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
    intent: Option<QueryIntent>,
    embed_model: Option<&dyn EmbedQueryFn>,
) -> Result<BrainContextResult, anyhow::Error> {
    build_brain_context_hybrid_with_aliases(
        store,
        inputs,
        tantivy,
        config,
        &std::collections::HashMap::new(),
        None,
        intent,
        embed_model,
        None,
    )
}

fn ensure_brain_context_not_cancelled(
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), anyhow::Error> {
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
        return Err(anyhow::Error::new(nestweaver_store::StoreError::Cancelled(
            nestweaver_store::CancelReason::Timeout,
        )));
    }
    Ok(())
}

/// Like `build_brain_context_hybrid` but also accepts a taxonomy alias map
/// (from `load_alias_sidecar`) for vault-defined name canonicalization.
///
/// When a seed does not resolve via title, symbol, or tag lookup, the alias
/// map is consulted: if the seed matches any alias in the map the corresponding
/// canonical name is tried in its place. This allows users to write natural-
/// language seeds (e.g. `"Auth"`) even when the note is titled
/// `"Authentication Service"`.
///
/// The optional `intent` parameter tunes PPR's damping factor and edge
/// weights. When `None`, the standard damping (0.85) is used.
#[allow(clippy::too_many_arguments)]
pub fn build_brain_context_hybrid_with_aliases(
    store: &GraphStore,
    inputs: &[String],
    tantivy: Option<&TantivyIndex>,
    config: &HybridSearchConfig,
    aliases: &std::collections::HashMap<String, Vec<String>>,
    db_path: Option<&std::path::Path>,
    intent: Option<QueryIntent>,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
            || trimmed.starts_with("proj:")
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

        // Fall back to symbol name search. Respect the caller's configured
        // [`HybridSearchConfig::seed_resolution`] (sourced from
        // `[seed_resolution]` in instance config) so user overrides actually
        // take effect at seed resolution.
        let symbol_matches = store
            .search_symbols_by_name(trimmed, 5, &config.seed_resolution)
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

        // Try project alias lookup via the extension sidecar.
        if let Some(db_path) = db_path {
            let ext_store = crate::extensions::load_extensions(db_path);
            let needle = trimmed.to_lowercase();
            let all_projects = store.list_projects().unwrap_or_default();
            let alias_matches: Vec<_> = all_projects
                .iter()
                .filter(|p| {
                    if let Some(serde_json::Value::Array(aliases)) =
                        ext_store.get(&p.uid).and_then(|m| m.get("aliases"))
                    {
                        aliases
                            .iter()
                            .any(|a| a.as_str().is_some_and(|s| s.to_lowercase() == needle))
                    } else {
                        false
                    }
                })
                .collect();
            if alias_matches.len() > 1 {
                let names: Vec<&str> = alias_matches.iter().map(|p| p.name.as_str()).collect();
                tracing::warn!(
                    alias = trimmed,
                    projects = %names.join(", "),
                    "Multiple projects match alias '{}': {}",
                    trimmed,
                    names.join(", "),
                );
            }
            let alias_project = alias_matches.into_iter().next().cloned();
            if let Some(project) = alias_project {
                if let Ok(note_uids) = store.list_project_note_uids(&project.uid) {
                    seed_uids.extend(note_uids);
                }
                if let Ok(sym_uids) = store.list_project_symbol_uids(&project.uid) {
                    seed_uids.extend(sym_uids);
                }
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

    // ── Semantic seed blending ────────────────────────────────────────────
    // When an embedding model is available and the semantic weight is nonzero
    // AND the store actually holds embedding vectors (otherwise the whole leg
    // is a wasted BERT forward pass — see `store_has_embeddings`), run a
    // vector KNN search over the entire graph. The top-k hits are
    // injected as PPR seeds (always-blend) so the walk explores semantically
    // relevant neighborhoods even when the textual seeds miss them. The full
    // hit list is passed to weighted_score_fuse as the semantic signal.
    let semantic_requested = config.weight_semantic > 0.0;
    let mut semantic_applied = false;
    let mut semantic_seed_count: usize = 0;
    let mut semantic_hits: Vec<(String, f64)> = Vec::new();
    if let Some(model) = embed_model
        && config.weight_semantic > 0.0
        // No vectors in the store → the semantic leg can contribute
        // nothing; skip the BERT forward pass entirely (perf: this made
        // every brain_context pay a full embed on un-embedded DBs).
        && store_has_embeddings(store)
    {
        let query_text = inputs.join(" ");
        ensure_brain_context_not_cancelled(cancel)?;
        let query_embedding = model.embed_query(&query_text);
        // Inference is a synchronous, potentially long-running boundary. A
        // timeout/disconnect can arrive while it is blocked, including on an
        // inference error. Preserve cancellation as an incomplete query rather
        // than degrading that error into a cacheable semantic miss.
        ensure_brain_context_not_cancelled(cancel)?;
        if let Ok(query_emb) = query_embedding {
            match crate::vector_search::vector_knn_all_cancellable(
                store,
                &query_emb,
                config.semantic_limit,
                cancel,
            ) {
                Ok(hits) => {
                    semantic_applied = true;
                    if config.always_blend_semantic {
                        for (uid, _score) in hits.iter().take(config.semantic_seed_limit) {
                            if !seed_uids.contains(uid) {
                                seed_uids.push(uid.clone());
                                // nw-102: remember these are nearest-neighbour
                                // guesses, not resolutions of the query text.
                                semantic_seed_count += 1;
                            }
                        }
                    }
                    semantic_hits = hits;
                }
                // A cancelled query (timeout / disconnect) is incomplete, not a
                // benign miss: propagate it so the caller returns an error the
                // cache refuses to store — never a truncated-but-"complete" Ok.
                // Other embedding/search failures still degrade gracefully to no
                // semantic signal (the original tolerant behavior).
                Err(e)
                    if e.downcast_ref::<nestweaver_store::StoreError>()
                        .is_some_and(|s| s.is_cancelled()) =>
                {
                    return Err(e);
                }
                Err(_) => {}
            }
        }
    }

    if seed_uids.is_empty() {
        anyhow::bail!(
            "No seeds resolved. Tried as UIDs, note titles, tags (with or without '#'), symbol names, and semantic search. Unresolved: {:?}",
            unresolved,
        );
    }

    // Run unified PPR with optional intent tuning.
    let damping = intent.map_or(0.85, |i| i.damping());
    // nw-181: this path already carries the daemon's disconnect/timeout flag,
    // so hand it to PPR — the most expensive stage — instead of letting a
    // 193k-node push loop run for a client that is already gone.
    let ppr = store
        .personalized_pagerank_with_intent_cancellable(
            &seed_uids,
            damping,
            20,
            &GraphScope::unified(),
            intent,
            cancel.map(|flag| flag.as_ref()),
        )
        .map_err(|e| anyhow::anyhow!(e))?;

    // ── Hybrid retrieval: fuse PPR + BM25 + semantic ──────────────────────
    //
    // Feature F7 (PRF half): when `config.prf` is set, the BM25 leg runs a
    // two-pass pseudo-relevance-feedback expansion (`search_prf`) instead of a
    // single-pass `search`. The mined expansion terms are surfaced on the
    // result for auditing.
    let mut expansion_terms: Vec<String> = Vec::new();
    let fused: Vec<(String, f64)> = if let Some(tantivy) = tantivy {
        let bm25_query = inputs.join(" ");
        let bm25_hits = if config.prf {
            match tantivy.search_prf(&bm25_query, config.bm25_limit, nestweaver_store_stoplist()) {
                Ok((hits, terms)) => {
                    expansion_terms = terms;
                    hits
                }
                Err(_) => tantivy
                    .search(&bm25_query, config.bm25_limit)
                    .unwrap_or_default(),
            }
        } else {
            tantivy
                .search(&bm25_query, config.bm25_limit)
                .unwrap_or_default()
        };
        weighted_score_fuse(
            &ppr,
            &bm25_hits,
            &semantic_hits,
            config.weight_ppr,
            config.weight_bm25,
            config.weight_semantic,
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

    // Cover cancellation after inference/vector work but before the completed
    // result crosses the engine boundary into single-flight/cache publication.
    ensure_brain_context_not_cancelled(cancel)?;
    Ok(BrainContextResult {
        seeds,
        connected,
        unresolved_seeds: unresolved,
        expansion_terms,
        semantic_applied,
        semantic_seed_count,
        degraded_components: if semantic_requested && !semantic_applied {
            vec!["semantic".to_string()]
        } else {
            Vec::new()
        },
    })
}

/// The PRF stoplist: the engine's built-in cross-domain [`crate::cross_domain::STOPLIST`].
///
/// Reused here so the store crate (which owns the PRF term-mining but cannot
/// depend on the engine) need not maintain a second stoplist — the engine
/// threads this list into `TantivyIndex::search_prf`.
pub fn nestweaver_store_stoplist() -> &'static [&'static str] {
    // The cross-domain STOPLIST is code-identifier oriented; PRF mines prose
    // (note/section text), so augment it with common English stopwords to keep
    // them out of expansion terms (query-drift guard). Combined once, leaked.
    static COMBINED: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    COMBINED.get_or_init(|| {
        const ENGLISH: &[&str] = &[
            "the", "and", "for", "with", "that", "this", "from", "are", "was", "were", "but",
            "not", "you", "your", "its", "into", "over", "per", "via", "has", "have", "had",
            "will", "would", "can", "could", "should", "than", "then", "them", "they", "their",
            "what", "when", "which", "while", "about", "also", "been", "being", "does", "done",
            "each", "more", "most", "much", "such", "some", "only", "other", "these", "those",
            "there", "here", "because", "between", "all", "any", "out", "use", "used", "uses",
        ];
        crate::cross_domain::STOPLIST
            .iter()
            .copied()
            .chain(ENGLISH.iter().copied())
            .collect()
    })
}

/// Tanh-based score normalization.
///
/// Divides each score by the median of non-zero scores, then applies `tanh`
/// to compress into (0, 1]. This is robust to outliers and works across
/// heterogeneous score distributions (PPR, BM25, cosine similarity).
fn tanh_normalize(scores: &[f64]) -> Vec<f64> {
    if scores.is_empty() {
        return vec![];
    }
    let nonzero: Vec<f64> = scores.iter().copied().filter(|s| *s > 0.0).collect();
    if nonzero.is_empty() {
        return vec![0.0; scores.len()];
    }
    let median = {
        let mut sorted = nonzero.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[(sorted.len() - 1) / 2]
    };
    let scale = if median > 0.0 { median } else { 1.0 };
    scores.iter().map(|s| (s / scale).tanh()).collect()
}

/// Weighted score fusion (convex combination) of PPR, BM25, and semantic signals.
///
/// Each signal's raw scores are normalized via [`tanh_normalize`], then combined
/// as: `score(d) = w_ppr * norm_ppr(d) + w_bm25 * norm_bm25(d) + w_semantic * norm_sem(d)`.
///
/// Returns a fused list sorted descending by combined score.
pub fn weighted_score_fuse(
    ppr: &[(String, f64)],
    bm25: &[nestweaver_store::SearchHit],
    semantic: &[(String, f64)],
    w_ppr: f64,
    w_bm25: f64,
    w_semantic: f64,
) -> Vec<(String, f64)> {
    use std::collections::HashMap;

    let mut ppr_scores: HashMap<&str, f64> = HashMap::new();
    for (uid, score) in ppr {
        ppr_scores.insert(uid, *score);
    }

    let mut bm25_scores: HashMap<&str, f64> = HashMap::new();
    for hit in bm25 {
        bm25_scores.insert(&hit.uid, hit.score as f64);
    }

    let mut sem_scores: HashMap<&str, f64> = HashMap::new();
    for (uid, score) in semantic {
        sem_scores.insert(uid, *score);
    }

    let mut all_uids: Vec<&str> = Vec::new();
    for uid in ppr_scores
        .keys()
        .chain(bm25_scores.keys())
        .chain(sem_scores.keys())
    {
        if !all_uids.contains(uid) {
            all_uids.push(uid);
        }
    }

    let ppr_raw: Vec<f64> = all_uids
        .iter()
        .map(|u| ppr_scores.get(u).copied().unwrap_or(0.0))
        .collect();
    let bm25_raw: Vec<f64> = all_uids
        .iter()
        .map(|u| bm25_scores.get(u).copied().unwrap_or(0.0))
        .collect();
    let sem_raw: Vec<f64> = all_uids
        .iter()
        .map(|u| sem_scores.get(u).copied().unwrap_or(0.0))
        .collect();

    let ppr_norm = tanh_normalize(&ppr_raw);
    let bm25_norm = tanh_normalize(&bm25_raw);
    let sem_norm = tanh_normalize(&sem_raw);

    let mut results: Vec<(String, f64)> = all_uids
        .iter()
        .enumerate()
        .map(|(i, uid)| {
            let score = w_ppr * ppr_norm[i] + w_bm25 * bm25_norm[i] + w_semantic * sem_norm[i];
            (uid.to_string(), score)
        })
        .collect();

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
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
pub(crate) fn render_brain_node(
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
                inline_body: None,
                body_complete: true,
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
                inline_body: None,
                body_complete: true,
            })),
            Err(nestweaver_store::StoreError::NotFound) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    } else if uid.starts_with("head:") {
        // Look up the Heading node for its text; fall back to UID.
        let (title, location) = match store.lookup_heading(uid) {
            Ok(h) => {
                let parent_path = store
                    .lookup_note(&h.note_uid)
                    .map(|n| n.file_path)
                    .unwrap_or_default();
                (h.text, parent_path)
            }
            Err(_) => (uid.to_string(), String::new()),
        };
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Heading".to_string(),
            title,
            location,
            relevance: score,
            inline_body: None,
            body_complete: true,
        }))
    } else if uid.starts_with("sec:") {
        // Look up the Section node and derive a readable title from the
        // parent note title + section body preview.
        let (title, location) = match store.lookup_section(uid) {
            Ok(sec) => {
                let parent_note = store.lookup_note(&sec.note_uid).ok();
                let heading_text = sec
                    .heading_uid
                    .as_deref()
                    .and_then(|h_uid| store.lookup_heading(h_uid).ok())
                    .map(|h| h.text);

                let title = if let Some(heading) = heading_text {
                    heading
                } else {
                    // No heading -- build "{parent_note_title} -- {first_60_chars}..."
                    let note_title = parent_note
                        .as_ref()
                        .map(|n| n.title.as_str())
                        .unwrap_or("Untitled");
                    let body_preview: String = sec
                        .text_content
                        .chars()
                        .take(60)
                        .collect::<String>()
                        .replace('\n', " ");
                    let ellipsis = if sec.text_content.len() > 60 {
                        "..."
                    } else {
                        ""
                    };
                    format!("{note_title} \u{2014} {body_preview}{ellipsis}")
                };

                let loc = parent_note.map(|n| n.file_path).unwrap_or_default();
                (title, loc)
            }
            Err(_) => (uid.to_string(), String::new()),
        };
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Section".to_string(),
            title,
            location,
            relevance: score,
            inline_body: None,
            body_complete: true,
        }))
    } else if uid.starts_with("tag:") {
        let tag_name = store
            .lookup_tag(uid)
            .map(|t| t.name)
            .unwrap_or_else(|_| uid.to_string());
        Ok(Some(BrainNode {
            uid: uid.to_string(),
            kind: "Tag".to_string(),
            title: if tag_name.is_empty() {
                uid.to_string()
            } else {
                tag_name
            },
            location: String::new(),
            relevance: score,
            inline_body: None,
            body_complete: true,
        }))
    } else {
        Ok(None)
    }
}

/// Feature F6 — per-path dampen/boost ranking priors.
///
/// Apply a continuous, query-independent prior on result relevance keyed by
/// file-path glob. For each node, the rule whose glob matches the node's
/// `location` and appears **last** in the merged (dampen-then-boost) rule list
/// wins (last-match-wins); the node's `relevance` is multiplied by that rule's
/// multiplier and the **final product** is clamped to
/// `[RANKING_MULTIPLIER_MIN, RANKING_MULTIPLIER_MAX]`. A node whose location
/// matches no rule is left unchanged.
///
/// Must be applied AFTER fusion (RRF is rank-only, so applying before it
/// no-ops) and BEFORE the caller's sort / truncation. Empty `rules` → no-op.
///
/// The matcher is rebuilt per call from `rules`; callers invoke this once per
/// result set so the cost is negligible. Invalid globs are skipped (and warned)
/// rather than failing the whole query.
pub fn apply_ranking_priors(nodes: &mut [BrainNode], rules: &RankingConfig) {
    if rules.is_empty() || nodes.is_empty() {
        return;
    }

    // Compile each rule's glob into a matcher, preserving order. A rule whose
    // glob fails to compile is dropped (logged) — one bad glob must not break
    // ranking for the rest.
    let ordered = rules.ordered_rules();
    let compiled: Vec<(globset::GlobMatcher, f64)> = ordered
        .iter()
        .filter_map(|rule| match globset::Glob::new(&rule.glob) {
            Ok(g) => Some((g.compile_matcher(), rule.multiplier)),
            Err(e) => {
                tracing::warn!(glob = %rule.glob, error = %e, "skipping invalid ranking glob");
                None
            }
        })
        .collect();
    if compiled.is_empty() {
        return;
    }

    for node in nodes.iter_mut() {
        // `location` may carry a trailing `:line` (Symbol nodes render as
        // `path:line`); strip it so the glob matches the path itself.
        let path = node
            .location
            .rsplit_once(':')
            .filter(|(_, line)| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
            .map(|(p, _)| p)
            .unwrap_or(node.location.as_str());

        // Last-match-wins: scan in order and keep the multiplier of the last
        // matching rule.
        let mut chosen: Option<f64> = None;
        for (matcher, multiplier) in &compiled {
            if matcher.is_match(path) {
                chosen = Some(*multiplier);
            }
        }

        if let Some(multiplier) = chosen {
            node.relevance =
                (node.relevance * multiplier).clamp(RANKING_MULTIPLIER_MIN, RANKING_MULTIPLIER_MAX);
        }
    }
}

/// Dry-run companion to [`apply_ranking_priors`] for a single location.
///
/// Given a node's file-path `location` and the ranking `rules`, returns the
/// last matching rule (its glob + multiplier, last-match-wins) — or `None` when
/// nothing matches — alongside the `final_relevance` that
/// [`apply_ranking_priors`] would produce from `base_relevance`. Used by the
/// `nestweaver ranking explain` CLI dry-run so the binary need not depend on
/// `globset` or duplicate the matching/clamping math.
pub fn explain_ranking_prior(
    location: &str,
    base_relevance: f64,
    rules: &RankingConfig,
) -> (Option<(String, f64)>, f64) {
    // Strip a trailing `:line` (Symbol locations render as `path:line`).
    let path = location
        .rsplit_once(':')
        .filter(|(_, line)| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
        .map(|(p, _)| p)
        .unwrap_or(location);

    let mut matched: Option<(String, f64)> = None;
    for rule in rules.ordered_rules() {
        match globset::Glob::new(&rule.glob) {
            Ok(g) if g.compile_matcher().is_match(path) => {
                matched = Some((rule.glob.clone(), rule.multiplier));
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(glob = %rule.glob, error = %e, "skipping invalid ranking glob");
            }
        }
    }

    let final_relevance = match &matched {
        Some((_, multiplier)) => {
            (base_relevance * multiplier).clamp(RANKING_MULTIPLIER_MIN, RANKING_MULTIPLIER_MAX)
        }
        None => base_relevance,
    };
    (matched, final_relevance)
}

/// Feature F8 — tiered inline bodies.
///
/// Populate `inline_body` on each node whose **normalized** relevance clears
/// `threshold`. Relevance is normalized against the maximum relevance in
/// `nodes`, so the threshold (e.g. 0.75) is meaningful regardless of the raw
/// PPR/RRF score scale. Bodies are truncated to `max_body_tokens`
/// (chars/4 estimate) and, when `token_budget` is `Some`, charged against the
/// budget in rank order: once the budget would be exceeded, lower-ranked nodes
/// are left metadata-only (`inline_body = None`) — the node itself is never
/// dropped.
///
/// Source of the body by kind:
/// - `Section` → the stored `text_content`.
/// - `Note` → the concatenated text of its sections.
/// - `Symbol/*` → the symbol's source span, read from `root` via the
///   `read_symbols` span logic.
/// - other kinds (Heading, Tag) → no inline body.
///
/// Callers opt in by calling this at all; it is never invoked on the
/// default (off) path.
/// Resolves a symbol's `repo_uid` to a [`ContentReader`] for reading its source
/// span. In server mode callers pass a resolver that yields a `GitBareReader`
/// for the repo's bare clone; in local mode they pass `None` and reads fall back
/// to a `FilesystemReader` rooted at `root`.
///
/// [`ContentReader`]: crate::content_reader::ContentReader
pub type InlineBodyReaderResolver<'a> =
    dyn Fn(&str) -> Option<Box<dyn crate::content_reader::ContentReader>> + 'a;

pub fn populate_inline_bodies(
    store: &GraphStore,
    nodes: &mut [BrainNode],
    root: &std::path::Path,
    threshold: f64,
    max_body_tokens: usize,
    token_budget: Option<usize>,
    reader_resolver: Option<&InlineBodyReaderResolver>,
) {
    let max_relevance = nodes.iter().map(|n| n.relevance).fold(0.0_f64, f64::max);
    if max_relevance <= 0.0 {
        return;
    }
    let max_body_chars = max_body_tokens.saturating_mul(4);
    let mut used_tokens = 0usize;

    for node in nodes.iter_mut() {
        let normalized = node.relevance / max_relevance;
        if normalized < threshold {
            continue;
        }
        let Some(body) = fetch_node_body(store, &node.uid, root, reader_resolver) else {
            continue;
        };
        if body.is_empty() {
            continue;
        }
        // Truncate to the per-body cap (chars/4 estimate). Bug H: prefer the
        // last newline within the cap so we never split a statement mid-line;
        // the returned flag is propagated into BrainNode.body_complete so
        // downstream consumers know whether the body is full or partial.
        let (body, complete) = truncate_body_to_chars(body, max_body_chars);
        // Token-budget gate: charge inline bodies ahead of metadata. The first
        // qualifying node is always allowed (mirrors read_symbols), so a single
        // oversized body never starves the whole result.
        if let Some(budget) = token_budget {
            let cost = estimate_tokens(&body);
            if used_tokens > 0 && used_tokens + cost > budget {
                continue;
            }
            used_tokens += cost;
        }
        node.inline_body = Some(body);
        node.body_complete = complete;
    }
}

/// Resolve a node UID to its source body for inline embedding. Returns `None`
/// for kinds without a meaningful body (Heading, Tag) or on lookup failure.
fn fetch_node_body(
    store: &GraphStore,
    uid: &str,
    root: &std::path::Path,
    reader_resolver: Option<&InlineBodyReaderResolver>,
) -> Option<String> {
    if uid.starts_with("sym:") {
        // In server mode the source lives in a bare clone, not on disk. Resolve
        // the symbol's repo and ask the caller-supplied resolver for the right
        // reader (a GitBareReader); only fall back to the filesystem when no
        // resolver is given (local mode) or the repo can't be resolved —
        // otherwise a FilesystemReader over a bare repo returns an empty body.
        if let Some(resolve) = reader_resolver
            && let Ok(sym) = store.lookup_symbol(uid)
            && let Some(reader) = resolve(&sym.repo_uid)
        {
            let res = crate::read_symbols::read_symbols(
                store,
                &[uid.to_string()],
                reader.as_ref(),
                0,
                None,
            );
            return res.symbols.into_iter().next().map(|w| w.body);
        }
        let reader = crate::content_reader::FilesystemReader::new(root);
        let res = crate::read_symbols::read_symbols(store, &[uid.to_string()], &reader, 0, None);
        return res.symbols.into_iter().next().map(|w| w.body);
    }
    if uid.starts_with("sec:") {
        return store.lookup_section(uid).ok().map(|s| s.text_content);
    }
    if uid.starts_with("note:") {
        let sections = store.sections_in_note(uid).ok()?;
        if sections.is_empty() {
            return None;
        }
        let mut combined: Vec<(u32, String)> = sections
            .into_iter()
            .map(|s| (s.start_line, s.text_content))
            .collect();
        combined.sort_by_key(|(line, _)| *line);
        return Some(
            combined
                .into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    None
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

/// Expand a search query by appending canonical names for any taxonomy alias
/// found as a whole word in the query.
///
/// For example, if the alias map contains `"Parallel Paths" → ["PP"]` and the
/// query is `"PP"`, the returned string is `"PP Parallel Paths"`. This bridges
/// the vocabulary gap so that brain_search finds notes titled with the canonical
/// name even when the user searches by abbreviation.
///
/// Rules:
/// - Only aliases with length >= 2 are considered (avoids single-letter noise).
/// - Matching is case-insensitive and whole-word only.
/// - At most 10 canonical expansions are appended (prevents query explosion).
/// - When no alias matches, the original query is returned unchanged.
pub fn expand_query_with_aliases(
    query: &str,
    aliases: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    if aliases.is_empty() {
        return query.to_string();
    }

    // Build reverse map: alias (lowercase) -> list of canonical names.
    let mut reverse: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (canonical, alias_list) in aliases {
        for alias in alias_list {
            let key = alias.to_lowercase();
            if key.len() >= 2 {
                reverse.entry(key).or_default().push(canonical.clone());
            }
        }
    }

    // Collect query words (lowercased) for whole-word matching.
    let query_words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();

    let mut expansions: Vec<String> = Vec::new();
    for (alias, canonicals) in &reverse {
        if query_words.iter().any(|w| w == alias) {
            for c in canonicals {
                if !expansions.contains(c) && expansions.len() < 10 {
                    expansions.push(c.clone());
                }
            }
        }
    }

    if expansions.is_empty() {
        return query.to_string();
    }
    format!("{} {}", query, expansions.join(" "))
}

#[cfg(test)]
mod render_brain_node_tests {
    use nestweaver_schema::{Note, NoteKind, Section};
    use nestweaver_store::GraphStore;

    use super::render_brain_node;

    #[test]
    fn section_with_heading_uses_heading_text() {
        let store = GraphStore::in_memory().unwrap();
        let note = Note {
            uid: "note:test-note".to_string(),
            vault_uid: "vault:test".to_string(),
            file_path: "notes/test.md".to_string(),
            title: "Test Note".to_string(),
            note_kind: NoteKind::General,
            word_count: 100,
            content_hash: "abc".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.insert_note(&note).unwrap();

        let heading = nestweaver_schema::Heading {
            uid: "head:test-heading".to_string(),
            note_uid: "note:test-note".to_string(),
            level: 2,
            text: "My Heading".to_string(),
            slug: "my-heading".to_string(),
            start_line: 1,
            end_line: 5,
            content_hash: "def".to_string(),
            embedding: None,
        };
        store.insert_heading(&heading).unwrap();

        let section = Section {
            uid: "sec:test-section-with-heading".to_string(),
            note_uid: "note:test-note".to_string(),
            heading_uid: Some("head:test-heading".to_string()),
            start_line: 1,
            end_line: 5,
            text_hash: "ghi".to_string(),
            text_content: "Some body text here".to_string(),
            word_count: 4,
            pagerank_score: None,
        };
        store.insert_section(&section).unwrap();

        let node = render_brain_node(&store, "sec:test-section-with-heading", 0.5)
            .unwrap()
            .expect("section should resolve");
        assert_eq!(node.title, "My Heading");
        assert_eq!(node.kind, "Section");
    }

    #[test]
    fn section_without_heading_falls_back_to_body_preview() {
        let store = GraphStore::in_memory().unwrap();
        let note = Note {
            uid: "note:fallback-note".to_string(),
            vault_uid: "vault:test".to_string(),
            file_path: "notes/fallback.md".to_string(),
            title: "Fallback Note".to_string(),
            note_kind: NoteKind::General,
            word_count: 50,
            content_hash: "abc2".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.insert_note(&note).unwrap();

        let section = Section {
            uid: "sec:test-section-no-heading".to_string(),
            note_uid: "note:fallback-note".to_string(),
            heading_uid: None,
            start_line: 0,
            end_line: 3,
            text_hash: "jkl".to_string(),
            text_content: "This is the preamble text before any heading".to_string(),
            word_count: 8,
            pagerank_score: None,
        };
        store.insert_section(&section).unwrap();

        let node = render_brain_node(&store, "sec:test-section-no-heading", 0.42)
            .unwrap()
            .expect("section should resolve");
        assert_eq!(node.kind, "Section");
        // Fallback title should be: "{note_title} -- {body_preview}"
        assert!(
            node.title.starts_with("Fallback Note \u{2014} "),
            "expected fallback title starting with note title; got: {}",
            node.title
        );
        assert!(
            node.title.contains("preamble"),
            "expected body preview in title; got: {}",
            node.title
        );
        assert_eq!(node.location, "notes/fallback.md");
    }
}

#[cfg(test)]
mod ranking_prior_tests {
    use super::{BrainNode, apply_ranking_priors};
    use crate::config::{GlobRule, RankingConfig};

    fn node(uid: &str, location: &str, relevance: f64) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: "Note".to_string(),
            title: uid.to_string(),
            location: location.to_string(),
            relevance,
            inline_body: None,
            body_complete: true,
        }
    }

    fn dampen(glob: &str, multiplier: f64) -> GlobRule {
        GlobRule {
            glob: glob.to_string(),
            multiplier,
        }
    }

    #[test]
    fn dampen_rule_reduces_matching_node_relevance() {
        let mut nodes = vec![node("note:old", "_logs/2020/jan.md", 1.0)];
        let rules = RankingConfig {
            dampen: vec![dampen("_logs/2020/**", 0.3)],
            boost: vec![],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut nodes, &rules);
        assert!(
            (nodes[0].relevance - 0.3).abs() < 1e-9,
            "expected 1.0 * 0.3 = 0.3, got {}",
            nodes[0].relevance
        );
    }

    #[test]
    fn node_matching_nothing_is_unchanged() {
        let mut nodes = vec![node("note:keep", "src/main.rs", 0.7)];
        let rules = RankingConfig {
            dampen: vec![dampen("_logs/2020/**", 0.3)],
            boost: vec![dampen("Projects/*/sync.md", 1.5)],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut nodes, &rules);
        assert!(
            (nodes[0].relevance - 0.7).abs() < 1e-9,
            "non-matching node must be unchanged, got {}",
            nodes[0].relevance
        );
    }

    #[test]
    fn last_matching_rule_wins() {
        // Two rules both match; the later one (boost, 2.0) must win over the
        // earlier dampen (0.3). dampen rules come first in the merged order.
        let mut nodes = vec![node("note:x", "Projects/app/sync.md", 1.0)];
        let rules = RankingConfig {
            dampen: vec![dampen("Projects/**", 0.3)],
            boost: vec![dampen("Projects/*/sync.md", 2.0)],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut nodes, &rules);
        assert!(
            (nodes[0].relevance - 2.0).abs() < 1e-9,
            "last matching rule (2.0) must win, got {}",
            nodes[0].relevance
        );
    }

    #[test]
    fn final_product_is_clamped_to_bounds() {
        // Boost product above the ceiling clamps to 5.0.
        let mut high = vec![node("note:hi", "critical/x.md", 4.0)];
        let rules_hi = RankingConfig {
            dampen: vec![],
            boost: vec![dampen("critical/**", 5.0)],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut high, &rules_hi);
        assert!(
            (high[0].relevance - 5.0).abs() < 1e-9,
            "4.0 * 5.0 must clamp to 5.0, got {}",
            high[0].relevance
        );

        // Dampen product below the floor clamps to 0.05.
        let mut low = vec![node("note:lo", "archive/x.md", 0.1)];
        let rules_lo = RankingConfig {
            dampen: vec![dampen("archive/**", 0.05)],
            boost: vec![],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut low, &rules_lo);
        assert!(
            (low[0].relevance - 0.05).abs() < 1e-9,
            "0.1 * 0.05 = 0.005 must clamp up to 0.05, got {}",
            low[0].relevance
        );
    }

    #[test]
    fn strips_trailing_line_number_from_symbol_location() {
        // Symbol nodes render location as `path:line`; the glob matches the path.
        let mut nodes = vec![node("sym:f", "src/legacy/foo.rs:42", 1.0)];
        let rules = RankingConfig {
            dampen: vec![dampen("src/legacy/**", 0.5)],
            boost: vec![],
            enable_prf: false,
            test_path_patterns: vec![],
            git_activity_weight: 1.2,
        };
        apply_ranking_priors(&mut nodes, &rules);
        assert!(
            (nodes[0].relevance - 0.5).abs() < 1e-9,
            "expected path glob to match despite :line suffix, got {}",
            nodes[0].relevance
        );
    }

    #[test]
    fn empty_config_is_noop() {
        let mut nodes = vec![node("note:x", "anything/here.md", 0.9)];
        apply_ranking_priors(&mut nodes, &RankingConfig::default());
        assert!((nodes[0].relevance - 0.9).abs() < 1e-9);
    }
}

#[cfg(test)]
mod inline_body_tests {
    use super::{BrainNode, populate_inline_bodies};
    use nestweaver_schema::{Note, NoteKind, Section};
    use nestweaver_store::GraphStore;
    use std::fs;

    fn node(uid: &str, kind: &str, relevance: f64) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: uid.to_string(),
            location: String::new(),
            relevance,
            inline_body: None,
            body_complete: true,
        }
    }

    fn store_with_section() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        let note = Note {
            uid: "note:n".to_string(),
            vault_uid: "vault:v".to_string(),
            file_path: "notes/n.md".to_string(),
            title: "N".to_string(),
            note_kind: NoteKind::General,
            word_count: 10,
            content_hash: "h".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store.insert_note(&note).unwrap();
        let sec = Section {
            uid: "sec:high".to_string(),
            note_uid: "note:n".to_string(),
            heading_uid: None,
            start_line: 0,
            end_line: 2,
            text_hash: "th".to_string(),
            text_content: "THE BODY OF THE HIGH RELEVANCE SECTION".to_string(),
            word_count: 7,
            pagerank_score: None,
        };
        store.insert_section(&sec).unwrap();
        let sec_lo = Section {
            uid: "sec:low".to_string(),
            note_uid: "note:n".to_string(),
            heading_uid: None,
            start_line: 3,
            end_line: 5,
            text_hash: "tl".to_string(),
            text_content: "low relevance body".to_string(),
            word_count: 3,
            pagerank_score: None,
        };
        store.insert_section(&sec_lo).unwrap();
        store
    }

    #[test]
    fn populates_above_threshold_only() {
        let store = store_with_section();
        // Max relevance is 1.0 → sec:high normalizes to 1.0 (>= 0.75),
        // sec:low normalizes to 0.5 (< 0.75).
        let mut nodes = vec![
            node("sec:high", "Section", 1.0),
            node("sec:low", "Section", 0.5),
        ];
        let root = std::env::temp_dir();
        populate_inline_bodies(&store, &mut nodes, &root, 0.75, 800, None, None);
        assert_eq!(
            nodes[0].inline_body.as_deref(),
            Some("THE BODY OF THE HIGH RELEVANCE SECTION"),
            "above-threshold node should have inline body"
        );
        assert!(
            nodes[1].inline_body.is_none(),
            "below-threshold node should NOT have inline body"
        );
    }

    #[test]
    fn off_by_default_no_call_means_none() {
        // Sanity: constructing a node leaves inline_body None; the populate
        // path is the only thing that sets it.
        let n = node("sec:high", "Section", 1.0);
        assert!(n.inline_body.is_none());
    }

    #[test]
    fn token_budget_leaves_lower_ranked_metadata_only() {
        let store = store_with_section();
        let mut nodes = vec![
            node("sec:high", "Section", 1.0),
            node("sec:low", "Section", 0.9),
        ];
        // Both normalize above threshold (1.0 and 0.9). Budget only fits the
        // first body (~10 tokens for 38 chars). The lower-ranked node keeps
        // its slot but gets no inline body.
        let root = std::env::temp_dir();
        populate_inline_bodies(&store, &mut nodes, &root, 0.75, 800, Some(11), None);
        assert!(nodes[0].inline_body.is_some(), "top node fits the budget");
        assert!(
            nodes[1].inline_body.is_none(),
            "lower-ranked node should be metadata-only once budget exhausts"
        );
        // Node itself is never dropped.
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn truncates_to_max_body_tokens() {
        let store = store_with_section();
        let mut nodes = vec![node("sec:high", "Section", 1.0)];
        // max_body_tokens = 2 → 8 chars.
        let root = std::env::temp_dir();
        populate_inline_bodies(&store, &mut nodes, &root, 0.75, 2, None, None);
        let body = nodes[0].inline_body.as_deref().unwrap();
        assert!(
            body.len() <= 8,
            "body should be truncated to ~8 chars, got {body:?}"
        );
    }

    #[test]
    fn symbol_body_read_from_disk() {
        use crate::index::index_directory_in_memory;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) {\n  return hello(name);\n}\n",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        let sym = store.lookup_symbols_by_name("greet").unwrap();
        let uid = sym[0].uid.clone();
        let mut nodes = vec![node(&uid, "Symbol/Function", 1.0)];
        populate_inline_bodies(&store, &mut nodes, &src, 0.75, 800, None, None);
        let body = nodes[0].inline_body.as_deref().expect("symbol body");
        assert!(body.contains("function greet"), "got: {body:?}");
    }

    /// In server mode the source lives in a bare clone with no working tree, so
    /// the default `FilesystemReader` path returns an empty body. Threading a
    /// reader resolver that yields a `GitBareReader` for the symbol's repo must
    /// populate the body from the bare clone instead.
    #[test]
    fn inline_bodies_populated_in_server_mode() {
        use crate::content_reader::{ContentReader, GitBareReader};
        use crate::index::index_directory_in_memory;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) {\n  return hello(name);\n}\n",
        )
        .unwrap();

        // Make `src` a git repo and bare-clone it — the server-mode source.
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .unwrap();
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        let bare = dir.path().join("repo.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                &src.display().to_string(),
                &bare.display().to_string(),
            ])
            .output()
            .unwrap();

        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        let uid = store.lookup_symbols_by_name("greet").unwrap()[0]
            .uid
            .clone();
        let repo_uid = store.lookup_symbol(&uid).unwrap().repo_uid;

        // Bug repro: with no resolver and a bare-clone "root" (no working tree),
        // the FilesystemReader path cannot read the source, so the body is empty.
        let mut nodes = vec![node(&uid, "Symbol/Function", 1.0)];
        populate_inline_bodies(&store, &mut nodes, &bare, 0.75, 800, None, None);
        assert!(
            nodes[0].inline_body.is_none(),
            "FilesystemReader over a bare clone should yield no body (the server-mode bug)"
        );

        // Fix: a resolver that yields a GitBareReader for the symbol's repo reads
        // the span straight from the bare clone.
        let bare_path = bare.clone();
        let expected_repo = repo_uid.clone();
        let resolver = move |ru: &str| -> Option<Box<dyn ContentReader>> {
            if ru == expected_repo {
                GitBareReader::from_head(&bare_path)
                    .ok()
                    .map(|r| Box::new(r) as Box<dyn ContentReader>)
            } else {
                None
            }
        };
        let mut nodes = vec![node(&uid, "Symbol/Function", 1.0)];
        populate_inline_bodies(&store, &mut nodes, &bare, 0.75, 800, None, Some(&resolver));
        let body = nodes[0]
            .inline_body
            .as_deref()
            .expect("server-mode symbol body should be read via GitBareReader");
        assert!(body.contains("function greet"), "got: {body:?}");
    }
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

#[cfg(test)]
mod expand_query_tests {
    use std::collections::HashMap;

    use super::expand_query_with_aliases;

    #[test]
    fn expands_matching_alias() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert(
            "Parallel Paths".to_string(),
            vec!["PP".to_string(), "ParPaths".to_string()],
        );
        let result = expand_query_with_aliases("PP", &aliases);
        assert!(
            result.starts_with("PP "),
            "should start with original query; got: {result}"
        );
        assert!(
            result.contains("Parallel Paths"),
            "should contain canonical name; got: {result}"
        );
    }

    #[test]
    fn no_match_returns_unchanged() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert("Authentication".to_string(), vec!["Auth".to_string()]);
        let result = expand_query_with_aliases("database migration", &aliases);
        assert_eq!(result, "database migration");
    }

    #[test]
    fn skips_short_aliases() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert("Something".to_string(), vec!["X".to_string()]);
        let result = expand_query_with_aliases("X", &aliases);
        // Single-char alias "X" should be skipped (len < 2).
        assert_eq!(result, "X");
    }

    #[test]
    fn case_insensitive_matching() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert("Authentication".to_string(), vec!["auth".to_string()]);
        let result = expand_query_with_aliases("Auth", &aliases);
        assert!(
            result.contains("Authentication"),
            "should match case-insensitively; got: {result}"
        );
    }

    #[test]
    fn caps_at_ten_expansions() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        // Create 15 canonical names all sharing the same alias.
        for i in 0..15 {
            aliases.insert(format!("Canonical{i}"), vec!["shared".to_string()]);
        }
        let result = expand_query_with_aliases("shared", &aliases);
        // Count space-separated tokens after the original query word.
        let expansion_count = result.split_whitespace().count() - 1; // minus "shared"
        assert!(
            expansion_count <= 10,
            "should cap at 10 expansions; got {expansion_count} in: {result}"
        );
    }

    #[test]
    fn empty_aliases_returns_unchanged() {
        let aliases: HashMap<String, Vec<String>> = HashMap::new();
        let result = expand_query_with_aliases("anything", &aliases);
        assert_eq!(result, "anything");
    }

    #[test]
    fn whole_word_only() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert("Authentication".to_string(), vec!["auth".to_string()]);
        // "authorize" contains "auth" as a substring but not as a whole word.
        let result = expand_query_with_aliases("authorize", &aliases);
        assert_eq!(
            result, "authorize",
            "should not match partial words; got: {result}"
        );
    }

    #[test]
    fn multiple_aliases_expand_independently() {
        let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
        aliases.insert("Authentication".to_string(), vec!["Auth".to_string()]);
        aliases.insert(
            "Device Pairing".to_string(),
            vec!["Pairing".to_string(), "BTP".to_string()],
        );
        let result = expand_query_with_aliases("Auth Pairing", &aliases);
        assert!(
            result.contains("Authentication"),
            "should expand Auth; got: {result}"
        );
        assert!(
            result.contains("Device Pairing"),
            "should expand Pairing; got: {result}"
        );
    }
}

#[cfg(test)]
mod dedup_heading_section_tests {
    use super::{BrainContextResult, BrainNode, dedup_heading_section_pairs};

    fn node(uid: &str, kind: &str, title: &str, loc: &str, rel: f64) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            location: loc.to_string(),
            relevance: rel,
            inline_body: None,
            body_complete: true,
        }
    }

    fn make(connected: Vec<BrainNode>) -> BrainContextResult {
        BrainContextResult {
            seeds: vec![],
            connected,
            unresolved_seeds: vec![],
            expansion_terms: vec![],
            semantic_applied: false,
            semantic_seed_count: 0,
            degraded_components: vec![],
        }
    }

    #[test]
    fn drops_heading_when_section_with_same_file_and_title_present() {
        let mut r = make(vec![
            node("h1", "Heading", "Overview", "notes/foo.md", 0.5),
            node("s1", "Section", "Overview", "notes/foo.md", 0.4),
        ]);
        dedup_heading_section_pairs(&mut r);
        assert_eq!(r.connected.len(), 1);
        assert_eq!(r.connected[0].uid, "s1");
    }

    #[test]
    fn keeps_heading_when_no_matching_section() {
        let mut r = make(vec![
            node("h1", "Heading", "Overview", "notes/foo.md", 0.5),
            node("s2", "Section", "Different Title", "notes/foo.md", 0.4),
        ]);
        dedup_heading_section_pairs(&mut r);
        assert_eq!(r.connected.len(), 2);
    }

    #[test]
    fn collapses_locations_with_line_or_anchor_suffix() {
        // Heading carries `file:line`, Section carries `file#anchor`; both
        // should collapse to the same file stem.
        let mut r = make(vec![
            node("h1", "Heading", "Setup", "notes/bar.md:42", 0.5),
            node("s1", "Section", "Setup", "notes/bar.md#setup", 0.4),
        ]);
        dedup_heading_section_pairs(&mut r);
        assert_eq!(r.connected.len(), 1);
        assert_eq!(r.connected[0].kind, "Section");
    }

    #[test]
    fn no_op_when_no_sections_present() {
        let mut r = make(vec![
            node("h1", "Heading", "A", "x.md", 0.5),
            node("h2", "Heading", "B", "x.md", 0.4),
        ]);
        dedup_heading_section_pairs(&mut r);
        assert_eq!(r.connected.len(), 2);
    }

    #[test]
    fn ignores_unrelated_kinds() {
        let mut r = make(vec![
            node("n1", "Note/PRD", "Spec", "notes/spec.md", 0.6),
            node("s1", "Section", "Spec", "notes/spec.md", 0.5),
        ]);
        dedup_heading_section_pairs(&mut r);
        // Note nodes are never dropped, only Heading nodes.
        assert_eq!(r.connected.len(), 2);
    }
}

#[cfg(test)]
mod fusion_tests {
    use super::*;
    use nestweaver_store::SearchHit;

    #[test]
    fn test_tanh_normalize_basic() {
        let scores = vec![0.5, 0.3, 0.1, 0.05, 0.01];
        let normalized = tanh_normalize(&scores);
        for v in &normalized {
            assert!(*v > 0.0 && *v <= 1.0);
        }
        for i in 1..normalized.len() {
            assert!(normalized[i - 1] >= normalized[i]);
        }
    }

    #[test]
    fn test_tanh_normalize_empty() {
        let normalized = tanh_normalize(&[]);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_tanh_normalize_all_zero() {
        let normalized = tanh_normalize(&[0.0, 0.0, 0.0]);
        assert_eq!(normalized, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_weighted_score_fuse_three_signals() {
        let ppr = vec![("a".to_string(), 0.5), ("b".to_string(), 0.3)];
        let bm25 = vec![
            SearchHit {
                uid: "b".to_string(),
                kind: "function".into(),
                title: "b".into(),
                vault_uid: "v".into(),
                note_uid: String::new(),
                score: 10.0,
            },
            SearchHit {
                uid: "c".to_string(),
                kind: "function".into(),
                title: "c".into(),
                vault_uid: "v".into(),
                note_uid: String::new(),
                score: 8.0,
            },
        ];
        let semantic = vec![("a".to_string(), 0.9), ("c".to_string(), 0.7)];

        let results = weighted_score_fuse(&ppr, &bm25, &semantic, 0.40, 0.25, 0.35);
        let uids: Vec<&str> = results.iter().map(|(u, _)| u.as_str()).collect();
        assert!(uids.contains(&"a"));
        assert!(uids.contains(&"b"));
        assert!(uids.contains(&"c"));
    }

    #[test]
    fn test_weighted_score_fuse_empty_semantic() {
        let ppr = vec![("a".to_string(), 0.5)];
        let bm25 = vec![SearchHit {
            uid: "a".to_string(),
            kind: "function".into(),
            title: "a".into(),
            vault_uid: "v".into(),
            note_uid: String::new(),
            score: 5.0,
        }];
        let results = weighted_score_fuse(&ppr, &bm25, &[], 0.70, 0.30, 0.0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "a");
    }
}

#[cfg(test)]
mod instance_validation_tests {
    use nestweaver_schema::{Project, Repo, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    use super::lookup_symbol;

    /// In-memory store holding one repo (with the given `instance_id`) plus a
    /// symbol living in it, mirroring the live DB where repos/symbols share a
    /// db-path-hash instance.
    fn store_with_repo_instance(instance_id: &str) -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        let repo = Repo {
            uid: "repo:1".to_string(),
            url: "https://github.com/example/repo".to_string(),
            indexed_sha: "abc123".to_string(),
            staleness_commits_behind: 0,
            instance_id: instance_id.to_string(),
            name: None,
            root_path: None,
        };
        store.insert_repo(&repo).unwrap();
        let symbol = Symbol {
            uid: "sym:1".to_string(),
            name: "anything".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 5,
            signature: "fn anything()".to_string(),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&symbol).unwrap();
        store
    }

    /// Insert a Project node carrying `instance_id` — models the third
    /// instance-id convention (logical config name) that `list-projects`
    /// surfaces to users.
    fn insert_test_project(store: &GraphStore, uid: &str, instance_id: &str) {
        let project = Project {
            uid: uid.to_string(),
            name: uid.to_string(),
            summary: None,
            instance_id: instance_id.to_string(),
        };
        store.insert_project(&project).unwrap();
    }

    #[test]
    fn unknown_instance_bails_with_valid_list() {
        let store = store_with_repo_instance("c37ccf01");
        let err = match lookup_symbol(&store, "anything", Some("nope")) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for an unknown instance"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown instance 'nope'"), "{msg}");
        assert!(msg.contains("c37ccf01"), "{msg}");
    }

    #[test]
    fn project_only_instance_gets_merge_guidance() {
        // A user who saw "Instance: kory-brain" in list-projects and typed it must
        // get the consolidation instruction, not a dead end.
        let store = store_with_repo_instance("c37ccf01");
        insert_test_project(&store, "proj", "kory-brain");
        let err = match lookup_symbol(&store, "anything", Some("kory-brain")) {
            Err(e) => e,
            Ok(_) => panic!("expected merge guidance for a project-only instance"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("names projects or vaults"), "{msg}");
        assert!(
            msg.contains("instance merge --from c37ccf01 --to kory-brain"),
            "{msg}"
        );
    }
}

#[cfg(test)]
mod semantic_leg_tests {
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    use super::{EmbedQueryFn, HybridSearchConfig, build_brain_context_hybrid_with_aliases};

    /// Embed model that records how often the (expensive) BERT forward pass
    /// ran, so tests can prove the semantic leg was skipped entirely.
    struct CountingEmbed {
        calls: std::sync::atomic::AtomicUsize,
    }

    struct WrongDimensionEmbed;
    struct FailingEmbed;
    struct BlockingFailingEmbed {
        inference_started: std::sync::Arc<std::sync::Barrier>,
        cancellation_published: std::sync::Arc<std::sync::Barrier>,
    }

    impl CountingEmbed {
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl EmbedQueryFn for CountingEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![0.0; 4])
        }
    }

    impl EmbedQueryFn for WrongDimensionEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![1.0, 0.0])
        }
    }

    impl EmbedQueryFn for FailingEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("probe inference failed")
        }
    }

    impl EmbedQueryFn for BlockingFailingEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.inference_started.wait();
            self.cancellation_published.wait();
            anyhow::bail!("inference failed after cancellation")
        }
    }

    fn store_with_symbol() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        let symbol = Symbol {
            uid: "sym:payment".to_string(),
            name: "Payment".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/payment.rs".to_string(),
            start_line: 1,
            end_line: 2,
            signature: "fn Payment()".to_string(),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&symbol).unwrap();
        store
    }

    fn run_context(
        store: &GraphStore,
        model: Option<&dyn EmbedQueryFn>,
        weight_semantic: f64,
    ) -> super::BrainContextResult {
        let config = HybridSearchConfig {
            weight_semantic,
            ..HybridSearchConfig::default()
        };
        build_brain_context_hybrid_with_aliases(
            store,
            &["Payment".to_string()],
            None,
            &config,
            &std::collections::HashMap::new(),
            None,
            None,
            model,
            None,
        )
        .expect("brain_context must preserve graph results")
    }

    #[test]
    fn semantic_leg_skipped_when_store_has_no_embeddings() {
        // Regression: brain_context used to pay a full BERT forward pass per
        // call even on databases with zero embedding vectors, where the
        // semantic leg can contribute nothing (p50 24-27s in debug builds).
        let store = store_with_symbol();
        assert_eq!(store.embedding_count(), 0);
        let model = CountingEmbed { calls: 0.into() };

        let result = run_context(&store, Some(&model), 0.35);

        assert_eq!(
            model.call_count(),
            0,
            "no embedding vectors in the DB → the semantic leg must not run the BERT embed"
        );
        assert!(!result.semantic_applied);
        assert_eq!(result.degraded_components, ["semantic"]);
    }

    #[test]
    fn semantic_leg_runs_when_store_has_embeddings() {
        // Control: once vectors exist, the semantic leg fires as before.
        let store = store_with_symbol();
        assert!(store.add_embedding("sym:payment", vec![0.0; 4]));
        let model = CountingEmbed { calls: 0.into() };

        let result = run_context(&store, Some(&model), 0.35);

        assert_eq!(
            model.call_count(),
            1,
            "embedding vectors present → the semantic leg must embed the query"
        );
        assert!(result.semantic_applied);
        assert!(result.degraded_components.is_empty());
    }

    #[test]
    fn requested_semantic_leg_degrades_when_model_is_not_ready() {
        let store = store_with_symbol();
        assert!(store.add_embedding("sym:payment", vec![0.0; 4]));

        let result = run_context(&store, None, 0.35);

        assert!(!result.semantic_applied);
        assert_eq!(result.degraded_components, ["semantic"]);
        assert!(
            !result.seeds.is_empty() || !result.connected.is_empty(),
            "graph/PPR results must survive"
        );
    }

    #[test]
    fn requested_semantic_leg_degrades_when_inference_fails() {
        let store = store_with_symbol();
        assert!(store.add_embedding("sym:payment", vec![0.0; 4]));

        let result = run_context(&store, Some(&FailingEmbed), 0.35);

        assert!(!result.semantic_applied);
        assert_eq!(result.degraded_components, ["semantic"]);
        assert!(
            !result.seeds.is_empty() || !result.connected.is_empty(),
            "graph/PPR results must survive"
        );
    }

    #[test]
    fn cancellation_during_failed_inference_is_not_degraded_to_success() {
        let store = store_with_symbol();
        assert!(store.add_embedding("sym:payment", vec![0.0; 4]));
        let inference_started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let cancellation_published = std::sync::Arc::new(std::sync::Barrier::new(2));
        let model = BlockingFailingEmbed {
            inference_started: inference_started.clone(),
            cancellation_published: cancellation_published.clone(),
        };
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_from_worker = cancel.clone();
        let canceller = std::thread::spawn(move || {
            inference_started.wait();
            cancel_from_worker.store(true, std::sync::atomic::Ordering::Release);
            cancellation_published.wait();
        });
        let config = HybridSearchConfig {
            weight_semantic: 0.35,
            ..HybridSearchConfig::default()
        };

        let err = build_brain_context_hybrid_with_aliases(
            &store,
            &["Payment".to_string()],
            None,
            &config,
            &std::collections::HashMap::new(),
            None,
            None,
            Some(&model),
            Some(&cancel),
        )
        .expect_err("cancellation during failed inference must remain a cancellation error");
        canceller.join().unwrap();

        assert!(
            err.downcast_ref::<nestweaver_store::StoreError>()
                .is_some_and(nestweaver_store::StoreError::is_cancelled),
            "expected StoreError::Cancelled, got: {err:#}"
        );
    }

    #[test]
    fn zero_semantic_weight_is_not_reported_as_degraded() {
        let store = store_with_symbol();

        let result = run_context(&store, None, 0.0);

        assert!(!result.semantic_applied);
        assert!(result.degraded_components.is_empty());
    }

    #[test]
    fn wrong_dimension_query_embedding_cannot_create_semantic_seeds() {
        let store = store_with_symbol();
        assert!(store.add_embedding("sym:payment", vec![1.0, 0.0, 0.0, 0.0]));
        let config = HybridSearchConfig {
            weight_semantic: 0.35,
            ..HybridSearchConfig::default()
        };

        let err = build_brain_context_hybrid_with_aliases(
            &store,
            &["not-a-real-seed".to_string()],
            None,
            &config,
            &std::collections::HashMap::new(),
            None,
            None,
            Some(&WrongDimensionEmbed),
            None,
        )
        .expect_err("wrong-dimensional query vectors must not inject semantic seeds");

        assert!(format!("{err:#}").contains("No seeds resolved"));
    }
}

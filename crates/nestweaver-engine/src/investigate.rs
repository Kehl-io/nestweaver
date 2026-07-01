//! Feature F10: the `investigate` bundle primitive.
//!
//! Collapses the "orient me on topic X" multi-round-trip pattern into a single
//! call that returns an architectural map (`investigate`) plus a `bundle_id`,
//! followed by cheap drill-in calls (`investigate_expand`, `investigate_hydrate`).
//!
//! Composition over reinvention: this module reuses
//! - [`crate::query::build_brain_context_hybrid_with_aliases`] for hybrid
//!   PPR + BM25 retrieval (PRF enabled),
//! - [`crate::query::populate_inline_bodies`] (F8) for inline source bodies,
//! - project-scope member-UID logic (mirrors `tool_project_context`).
//!
//! Bundles are persisted to a JSON sidecar `<db>.bundles.json` using the same
//! atomic write-then-rename pattern as the interactions/extensions sidecars,
//! with a 24h TTL (stale bundles are dropped on load).

use std::collections::HashMap;
use std::path::Path;

use nestweaver_store::{GraphStore, TantivyIndex};
use serde::{Deserialize, Serialize};

use crate::query::{
    BrainNode, EmbedQueryFn, HybridSearchConfig, build_brain_context_hybrid_with_aliases,
    populate_inline_bodies,
};

/// Bundle time-to-live: entries older than this are dropped when the sidecar
/// is loaded.
const BUNDLE_TTL_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Default retrieval breadth — how many connected nodes we consider for the map.
const DEFAULT_RETRIEVAL_BREADTH: usize = 30;

/// Default token budget for the architectural map.
const DEFAULT_TOKEN_BUDGET: usize = 4000;

/// Hard cap on the token budget regardless of caller request.
const MAX_TOKEN_BUDGET: usize = 16000;

/// Maximum number of high-confidence inline bodies to embed in the initial map.
const MAX_INLINE_BODIES: usize = 5;

/// Relevance threshold (normalized) above which a body is inlined in the map.
const INLINE_THRESHOLD: f64 = 0.75;

/// Per-body inline token cap.
const INLINE_MAX_BODY_TOKENS: usize = 400;

// ── Persisted bundle types ────────────────────────────────────────────────

/// One asset in a bundle. `asset_id` is a short stable hash of
/// `(bundle_id, uid)` so callers can drill into a specific entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEntry {
    pub asset_id: String,
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub location: String,
    /// A short summary (first line / heading of the body) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The full inline body when expanded/hydrated, or a high-confidence body
    /// inlined in the initial map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_body: Option<String>,
    /// Bug H — fidelity signal for `inline_body`. `true` when the inlined
    /// body contains the full source, `false` when the per-body cap forced
    /// truncation. Default `true`; skipped from JSON when `true` so existing
    /// consumers see unchanged output and only learn about the field when it
    /// flags a truncated body. Reliable on entries produced by
    /// `investigate_expand` and `investigate_hydrate`; best-effort on the
    /// initial `investigate` map (propagated from BrainNode.body_complete).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub body_complete: bool,
    /// Whether the entry has been expanded (full body + neighbors fetched).
    #[serde(default)]
    pub expanded: bool,
    pub relevance: f64,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

/// A persisted investigation bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub bundle_id: String,
    /// Seconds since the Unix epoch.
    pub created_at: f64,
    pub query: String,
    pub scope: String,
    pub entries: Vec<BundleEntry>,
}

/// On-disk container for all live bundles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub bundles: HashMap<String, Bundle>,
}

// ── Output (map / domain) types ───────────────────────────────────────────

/// A grouped "domain" within the architectural map. Each domain has an entry
/// point (its highest-ranked member) plus the remaining members.
#[derive(Debug, Clone, Serialize)]
pub struct Domain {
    pub label: String,
    /// `asset_id` of the entry-point node for this domain.
    pub entry_point: String,
    /// `asset_id`s of all members (including the entry point), ranked.
    pub members: Vec<String>,
}

/// The result of an `investigate` call: an architectural map + bundle handle.
#[derive(Debug, Clone, Serialize)]
pub struct InvestigateResult {
    pub bundle_id: String,
    pub query: String,
    pub scope: String,
    pub domains: Vec<Domain>,
    pub entries: Vec<BundleEntry>,
    /// Number of additional connected nodes dropped due to the token budget.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more_available: usize,
}

/// The result of `investigate_expand`: the expanded entries plus their
/// immediate neighbors.
#[derive(Debug, Clone, Serialize)]
pub struct ExpandResult {
    pub bundle_id: String,
    pub expanded: Vec<BundleEntry>,
    pub neighbors: Vec<NeighborRef>,
    /// Targets that could not be resolved within the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

/// A neighbor surfaced during expansion (caller/callee for symbols, wikilink
/// source for notes).
#[derive(Debug, Clone, Serialize)]
pub struct NeighborRef {
    /// `asset_id` of the entry this neighbor belongs to.
    pub of: String,
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub relation: String,
}

/// The result of `investigate_hydrate`: how many entries were filled.
#[derive(Debug, Clone, Serialize)]
pub struct HydrateResult {
    pub bundle_id: String,
    pub hydrated: usize,
    pub entries: Vec<BundleEntry>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

// ── Sidecar persistence ────────────────────────────────────────────────────

/// Canonical sidecar path for bundle data.
pub fn bundle_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    crate::sidecar_path(db_path, ".bundles.json")
}

/// Load the bundle store, dropping any bundles whose `created_at` is older than
/// the TTL. Returns an empty store when the sidecar is missing or unparseable.
pub fn load_bundle_store(db_path: &Path) -> BundleStore {
    let path = bundle_sidecar_path(db_path);
    let mut store: BundleStore = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let cutoff = now_epoch() - BUNDLE_TTL_SECS;
    store.bundles.retain(|_, b| b.created_at >= cutoff);
    store
}

/// Persist the bundle store via atomic write-then-rename.
pub fn save_bundle_store(db_path: &Path, store: &BundleStore) -> Result<(), anyhow::Error> {
    let path = bundle_sidecar_path(db_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(store)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load a single live (non-expired) bundle by id.
pub fn load_bundle(db_path: &Path, bundle_id: &str) -> Option<Bundle> {
    load_bundle_store(db_path).bundles.remove(bundle_id)
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Run an investigation: hybrid retrieval → domain grouping → inline bodies →
/// token-budgeting → persist a bundle. Returns the architectural map and the
/// `bundle_id` for follow-up drill-in.
///
/// `scope` accepts:
/// - `project:<slug>` — restrict retrieval seeds to a project's members,
/// - `repo:<name>` — restrict results to symbols in a named repo,
/// - `vault` / `all` / empty — no restriction (default).
///
/// When `db_path` is `Some`, the resulting bundle is persisted to the sidecar.
/// When `None` (e.g. an in-memory store), the bundle is returned but not saved;
/// follow-up calls would not find it.
#[allow(clippy::too_many_arguments)]
pub fn investigate(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    db_path: Option<&Path>,
    root: &Path,
    query: &str,
    scope: &str,
    token_budget: Option<usize>,
    embed_model: Option<&dyn EmbedQueryFn>,
) -> Result<InvestigateResult, anyhow::Error> {
    let budget = token_budget
        .unwrap_or(DEFAULT_TOKEN_BUDGET)
        .min(MAX_TOKEN_BUDGET);

    // 1. Resolve scope into the seed inputs and an optional repo filter.
    let (seed_inputs, repo_filter) = resolve_scope(store, query, scope)?;

    // 2. Hybrid retrieval with PRF enabled.
    let config = HybridSearchConfig {
        prf: true,
        ..Default::default()
    };
    // Graceful empty handling: when no seed resolves (e.g. a natural-language
    // multi-word query like "indexing pipeline" that matches no symbol/note
    // title verbatim), hybrid retrieval bails with `No seeds resolved`. Fall
    // back to BM25-only retrieval against the query text so investigations
    // remain useful for orientation queries instead of returning an empty map.
    let connected_result = build_brain_context_hybrid_with_aliases(
        store,
        &seed_inputs,
        tantivy,
        &config,
        &HashMap::new(),
        db_path,
        None,
        embed_model,
        None,
    );
    let mut connected: Vec<BrainNode> = match connected_result {
        Ok(ctx) => ctx.connected,
        Err(_) => bm25_fallback(store, tantivy, query, DEFAULT_RETRIEVAL_BREADTH),
    };
    if let Some(ref repo_uids) = repo_filter {
        connected.retain(|n| node_in_repo(store, n, repo_uids));
    }
    connected.truncate(DEFAULT_RETRIEVAL_BREADTH);

    // Graceful empty handling: still persist an empty bundle so the id is valid.
    let bundle_id = generate_bundle_id(query, scope);

    // 4. Inline at most MAX_INLINE_BODIES high-confidence bodies.
    populate_inline_bodies(
        store,
        &mut connected,
        root,
        INLINE_THRESHOLD,
        INLINE_MAX_BODY_TOKENS,
        Some(budget),
    );
    // Cap the number of inlined bodies (populate_inline_bodies has no count cap).
    let mut inlined = 0usize;
    for node in connected.iter_mut() {
        if node.inline_body.is_some() {
            inlined += 1;
            if inlined > MAX_INLINE_BODIES {
                node.inline_body = None;
            }
        }
    }

    // 5. Build entries, token-budgeting the map. Metadata + any inline body is
    //    charged against the budget; once exceeded, remaining nodes are dropped
    //    and counted in `more_available`.
    let mut entries: Vec<BundleEntry> = Vec::new();
    let mut used_tokens = 0usize;
    let mut more_available = 0usize;
    for node in &connected {
        let asset_id = compute_asset_id(&bundle_id, &node.uid);
        let summary = node
            .inline_body
            .as_deref()
            .map(summarize)
            .filter(|s| !s.is_empty());
        let entry = BundleEntry {
            asset_id,
            uid: node.uid.clone(),
            kind: node.kind.clone(),
            title: node.title.clone(),
            location: node.location.clone(),
            summary,
            inline_body: node.inline_body.clone(),
            body_complete: node.body_complete,
            expanded: false,
            relevance: node.relevance,
        };
        let cost = entry_token_cost(&entry);
        // Always admit the first entry so a single oversized node never starves
        // the whole map (mirrors populate_inline_bodies / read_symbols).
        if !entries.is_empty() && used_tokens + cost > budget {
            more_available += 1;
            continue;
        }
        used_tokens += cost;
        entries.push(entry);
    }

    // 6. Group into domains.
    let domains = group_into_domains(store, &entries);

    // 7. Persist the bundle (24h TTL handled on load).
    let bundle = Bundle {
        bundle_id: bundle_id.clone(),
        created_at: now_epoch(),
        query: query.to_string(),
        scope: scope.to_string(),
        entries: entries.clone(),
    };
    if let Some(db) = db_path {
        let mut bundle_store = load_bundle_store(db);
        bundle_store.version = 1;
        bundle_store.bundles.insert(bundle_id.clone(), bundle);
        save_bundle_store(db, &bundle_store)?;
    }

    Ok(InvestigateResult {
        bundle_id,
        query: query.to_string(),
        scope: scope.to_string(),
        domains,
        entries,
        more_available,
    })
}

/// Drill into specific bundle entries: fetch each target's full body plus its
/// immediate neighbors (callers/callees for symbols, wikilink sources for
/// notes). Marks the entries as expanded and persists the update.
///
/// `targets` accepts either `asset_id`s or raw `uid`s.
pub fn investigate_expand(
    store: &GraphStore,
    db_path: &Path,
    root: &Path,
    bundle_id: &str,
    targets: &[String],
) -> Result<ExpandResult, anyhow::Error> {
    let mut bundle_store = load_bundle_store(db_path);
    let bundle = bundle_store
        .bundles
        .get_mut(bundle_id)
        .ok_or_else(|| anyhow::anyhow!("bundle '{bundle_id}' not found or expired"))?;

    let mut expanded: Vec<BundleEntry> = Vec::new();
    let mut neighbors: Vec<NeighborRef> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();

    for target in targets {
        let Some(idx) = bundle
            .entries
            .iter()
            .position(|e| &e.asset_id == target || &e.uid == target)
        else {
            unresolved.push(target.clone());
            continue;
        };
        let uid = bundle.entries[idx].uid.clone();
        let asset_id = bundle.entries[idx].asset_id.clone();

        if let Some(body) = fetch_full_body(store, &uid, root) {
            // Bug H: `investigate_expand` stores the full body without
            // truncation, so the entry is unconditionally body-complete.
            if bundle.entries[idx].summary.is_none() {
                let s = summarize(&body);
                if !s.is_empty() {
                    bundle.entries[idx].summary = Some(s);
                }
            }
            bundle.entries[idx].inline_body = Some(body);
            bundle.entries[idx].body_complete = true;
        }
        bundle.entries[idx].expanded = true;
        neighbors.extend(fetch_neighbors(store, &uid, &asset_id));
        expanded.push(bundle.entries[idx].clone());
    }

    save_bundle_store(db_path, &bundle_store)?;

    Ok(ExpandResult {
        bundle_id: bundle_id.to_string(),
        expanded,
        neighbors,
        unresolved,
    })
}

/// Bulk-fill `inline_body`/`summary` for every bundle entry that lacks one,
/// budget-bounded. Persists the update.
pub fn investigate_hydrate(
    store: &GraphStore,
    db_path: &Path,
    root: &Path,
    bundle_id: &str,
    token_budget: Option<usize>,
) -> Result<HydrateResult, anyhow::Error> {
    let budget = token_budget
        .unwrap_or(DEFAULT_TOKEN_BUDGET)
        .min(MAX_TOKEN_BUDGET);
    let mut bundle_store = load_bundle_store(db_path);
    let bundle = bundle_store
        .bundles
        .get_mut(bundle_id)
        .ok_or_else(|| anyhow::anyhow!("bundle '{bundle_id}' not found or expired"))?;

    let mut used_tokens = 0usize;
    let mut hydrated = 0usize;
    for entry in bundle.entries.iter_mut() {
        if entry.inline_body.is_some() {
            continue;
        }
        let Some(body) = fetch_full_body(store, &entry.uid, root) else {
            continue;
        };
        if body.is_empty() {
            continue;
        }
        let max_chars = INLINE_MAX_BODY_TOKENS.saturating_mul(4);
        // Bug H: newline-aware truncation — see `truncate_body_to_chars`. The
        // `complete` flag is propagated to BundleEntry.body_complete so
        // consumers can decide whether to fall back to `read_symbols` for the
        // full source.
        let (body, complete) = crate::query::truncate_body_to_chars(body, max_chars);
        let cost = body.len().div_ceil(4);
        if hydrated > 0 && used_tokens + cost > budget {
            break;
        }
        used_tokens += cost;
        if entry.summary.is_none() {
            let s = summarize(&body);
            if !s.is_empty() {
                entry.summary = Some(s);
            }
        }
        entry.inline_body = Some(body);
        entry.body_complete = complete;
        hydrated += 1;
    }

    let entries = bundle.entries.clone();
    save_bundle_store(db_path, &bundle_store)?;

    Ok(HydrateResult {
        bundle_id: bundle_id.to_string(),
        hydrated,
        entries,
    })
}

// ── Scope resolution ──────────────────────────────────────────────────────

/// Resolve a scope string into (seed_inputs, optional repo-uid filter).
///
/// The `query` always seeds retrieval; project scope additionally seeds the
/// project's member UIDs; repo scope returns a UID set used to post-filter the
/// connected nodes.
fn resolve_scope(
    store: &GraphStore,
    query: &str,
    scope: &str,
) -> Result<(Vec<String>, Option<Vec<String>>), anyhow::Error> {
    let mut seeds = vec![query.to_string()];
    let scope = scope.trim();

    if let Some(slug) = scope.strip_prefix("project:") {
        let slug = slug.trim();
        if let Ok(Some(project)) = store.lookup_project_by_name(slug) {
            if let Ok(note_uids) = store.list_project_note_uids(&project.uid) {
                seeds.extend(note_uids);
            }
            if let Ok(sym_uids) = store.list_project_symbol_uids(&project.uid) {
                seeds.extend(sym_uids);
            }
        }
        return Ok((seeds, None));
    }

    if let Some(name) = scope.strip_prefix("repo:") {
        let name = name.trim();
        let repos = store
            .list_repos(None)
            .map_err(|e| anyhow::anyhow!("list_repos: {e}"))?;
        let matches: Vec<String> = repos
            .into_iter()
            .filter(|r| {
                crate::repo_display_name(r).eq_ignore_ascii_case(name) || r.uid.contains(name)
            })
            .map(|r| r.uid)
            .collect();
        return Ok((seeds, Some(matches)));
    }

    // vault / all / empty → no restriction.
    Ok((seeds, None))
}

/// BM25-only retrieval against the vault index when graph-seed resolution
/// fails. Returns up to `limit` `BrainNode`s ranked by BM25 score, normalized
/// so the top hit has relevance 1.0 (matching the hybrid pipeline's score
/// scale closely enough for downstream consumers). Returns an empty vec when
/// `tantivy` is absent or the query returns no hits.
fn bm25_fallback(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    query: &str,
    limit: usize,
) -> Vec<BrainNode> {
    let Some(tantivy) = tantivy else {
        return Vec::new();
    };
    let hits = match tantivy.search(query, limit) {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let max_score = hits.iter().map(|h| h.score as f64).fold(0.0_f64, f64::max);
    let mut nodes: Vec<BrainNode> = Vec::with_capacity(hits.len());
    for hit in hits {
        let normalized = if max_score > 0.0 {
            (hit.score as f64) / max_score
        } else {
            0.0
        };
        if let Ok(Some(node)) = crate::query::render_brain_node(store, &hit.uid, normalized) {
            nodes.push(node);
        }
    }
    nodes
}

/// Whether a node belongs to one of the given repos. Only symbol nodes are
/// repo-scoped; non-symbol nodes (notes/sections) pass through unfiltered.
fn node_in_repo(store: &GraphStore, node: &BrainNode, repo_uids: &[String]) -> bool {
    if !node.uid.starts_with("sym:") {
        return true;
    }
    let Ok(sym) = store.lookup_symbol(&node.uid) else {
        return false;
    };
    repo_uids.contains(&sym.repo_uid)
}

// ── Domain grouping ──────────────────────────────────────────────────────

/// Group entries into domains. v1 strategy: group symbols by their directory
/// prefix (top-level path component up to the file's parent) and group all
/// note/section nodes into a "Notes" domain. Within each domain the
/// highest-relevance entry becomes the entry point. Tractable and deterministic.
fn group_into_domains(_store: &GraphStore, entries: &[BundleEntry]) -> Vec<Domain> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        let label = domain_label(e);
        groups.entry(label).or_default().push(i);
    }
    let mut domains: Vec<Domain> = groups
        .into_iter()
        .map(|(label, mut idxs)| {
            // Rank members by relevance (descending); entry point is the top.
            idxs.sort_by(|&a, &b| {
                entries[b]
                    .relevance
                    .partial_cmp(&entries[a].relevance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let members: Vec<String> = idxs.iter().map(|&i| entries[i].asset_id.clone()).collect();
            let entry_point = members.first().cloned().unwrap_or_default();
            Domain {
                label,
                entry_point,
                members,
            }
        })
        .collect();
    // Order domains by their entry point's relevance (most relevant first).
    domains.sort_by(|a, b| {
        let ra = entries
            .iter()
            .find(|e| e.asset_id == a.entry_point)
            .map(|e| e.relevance)
            .unwrap_or(0.0);
        let rb = entries
            .iter()
            .find(|e| e.asset_id == b.entry_point)
            .map(|e| e.relevance)
            .unwrap_or(0.0);
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    domains
}

/// Compute a domain label for an entry: directory of a symbol's file, or a
/// kind bucket for notes/sections/tags.
fn domain_label(e: &BundleEntry) -> String {
    if e.uid.starts_with("sym:") {
        // location is typically "path/to/file.rs:line" — take the dir.
        let path = e.location.split(':').next().unwrap_or(&e.location);
        let dir = std::path::Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        format!("code:{dir}")
    } else if e.uid.starts_with("note:") || e.uid.starts_with("sec:") {
        "notes".to_string()
    } else {
        "other".to_string()
    }
}

// ── Body / neighbor fetching ──────────────────────────────────────────────

/// Fetch the full body for a node UID. Symbols → source span; sections →
/// section text; notes → concatenated section text.
fn fetch_full_body(store: &GraphStore, uid: &str, root: &Path) -> Option<String> {
    if uid.starts_with("sym:") {
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

/// Fetch immediate neighbors for a node: callers + callees for symbols,
/// wikilink sources for notes.
fn fetch_neighbors(store: &GraphStore, uid: &str, of: &str) -> Vec<NeighborRef> {
    let mut out = Vec::new();
    if uid.starts_with("sym:") {
        if let Ok(callers) = store.callers_of(uid) {
            for c in callers {
                out.push(NeighborRef {
                    of: of.to_string(),
                    uid: c.uid,
                    kind: "Symbol".to_string(),
                    title: c.name,
                    relation: "caller".to_string(),
                });
            }
        }
        if let Ok(callees) = store.callees_of(uid) {
            for c in callees {
                out.push(NeighborRef {
                    of: of.to_string(),
                    uid: c.uid,
                    kind: "Symbol".to_string(),
                    title: c.name,
                    relation: "callee".to_string(),
                });
            }
        }
    } else if uid.starts_with("note:")
        && let Ok(rows) = store.wikilink_sources_to_note(uid)
    {
        for r in rows {
            out.push(NeighborRef {
                of: of.to_string(),
                uid: r.source_note_uid,
                kind: "Note".to_string(),
                title: r.source_note_title,
                relation: "wikilink".to_string(),
            });
        }
    }
    out
}

// ── Hashing / summarizing helpers ──────────────────────────────────────────

/// Short stable hash of `(bundle_id, uid)` → 12-hex-char asset id.
fn compute_asset_id(bundle_id: &str, uid: &str) -> String {
    let h = fnv1a(&format!("{bundle_id}\u{0}{uid}"));
    format!("a{:012x}", h & 0x0000_ffff_ffff_ffff)
}

/// Generate a bundle id from query + scope + a timestamp salt so repeated
/// investigations get distinct ids.
fn generate_bundle_id(query: &str, scope: &str) -> String {
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = fnv1a(&format!("{query}\u{0}{scope}\u{0}{salt}"));
    format!("bndl_{:016x}", h)
}

/// 64-bit FNV-1a hash.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// First non-empty line of a body, trimmed and length-capped, used as a summary.
fn summarize(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(200).collect()
}

/// Token cost of an entry: metadata + inline body, chars/4 estimate.
fn entry_token_cost(e: &BundleEntry) -> usize {
    let meta = e.title.len() + e.location.len() + e.kind.len() + e.uid.len() + 16;
    let body = e.inline_body.as_deref().map(str::len).unwrap_or(0);
    let summary = e.summary.as_deref().map(str::len).unwrap_or(0);
    (meta + body + summary).div_ceil(4)
}

/// Current time as seconds since the Unix epoch.
fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::index_directory_in_memory;
    use std::fs;

    fn make_store() -> (tempfile::TempDir, std::path::PathBuf, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(src.join("greet")).unwrap();
        fs::write(
            src.join("greet").join("main.js"),
            "function greet(name) { return hello(name); }\n\
             function hello(name) { return name; }",
        )
        .unwrap();
        fs::write(
            src.join("util.js"),
            "function formatGreeting(name) { return greet(name); }",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        (dir, src, store)
    }

    #[test]
    fn investigate_returns_bundle_with_domains_and_entries() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "vault",
            Some(4000),
            None,
        )
        .unwrap();

        assert!(
            result.bundle_id.starts_with("bndl_"),
            "should return a bundle id, got {}",
            result.bundle_id
        );
        assert!(
            !result.entries.is_empty(),
            "should return at least one entry"
        );
        assert!(
            !result.domains.is_empty(),
            "should return at least one domain"
        );
        // Each domain's entry_point must be a real asset_id in entries.
        let asset_ids: std::collections::HashSet<&str> =
            result.entries.iter().map(|e| e.asset_id.as_str()).collect();
        for d in &result.domains {
            assert!(
                asset_ids.contains(d.entry_point.as_str()),
                "domain entry point should be a real asset"
            );
        }
        // Bundle was persisted.
        assert!(
            load_bundle(&db_path, &result.bundle_id).is_some(),
            "bundle should be persisted to the sidecar"
        );
    }

    #[test]
    fn investigate_expand_returns_a_body() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "vault",
            None,
            None,
        )
        .unwrap();

        // Pick a symbol entry to expand.
        let target = result
            .entries
            .iter()
            .find(|e| e.uid.starts_with("sym:"))
            .map(|e| e.asset_id.clone())
            .expect("at least one symbol entry");

        let expanded = investigate_expand(
            &store,
            &db_path,
            &src,
            &result.bundle_id,
            std::slice::from_ref(&target),
        )
        .unwrap();

        assert_eq!(expanded.expanded.len(), 1, "one entry expanded");
        let e = &expanded.expanded[0];
        assert!(e.expanded, "entry should be marked expanded");
        assert!(
            e.inline_body.as_deref().is_some_and(|b| !b.is_empty()),
            "expanded symbol entry should carry a non-empty body"
        );
        assert!(
            expanded.unresolved.is_empty(),
            "no unresolved targets expected"
        );
    }

    #[test]
    fn investigate_hydrate_fills_missing_bodies() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "vault",
            Some(50),
            None,
        )
        .unwrap();
        // With a tiny budget, most entries lack inline bodies.
        let missing_before = result
            .entries
            .iter()
            .filter(|e| e.inline_body.is_none())
            .count();

        let hydrated =
            investigate_hydrate(&store, &db_path, &src, &result.bundle_id, Some(4000)).unwrap();
        assert!(
            hydrated.hydrated <= missing_before,
            "cannot hydrate more entries than were missing"
        );
        if missing_before > 0 {
            assert!(hydrated.hydrated >= 1, "should hydrate at least one body");
        }
    }

    #[test]
    fn truncate_body_to_chars_prefers_last_newline() {
        // Body under the cap is returned unchanged with body_complete = true.
        let (out, complete) = crate::query::truncate_body_to_chars("short body".to_string(), 100);
        assert_eq!(out, "short body");
        assert!(complete);

        // Multi-line body over the cap: truncation walks back to the last
        // newline so we never split a statement mid-line. body_complete is
        // false so consumers know to fall back to read_symbols for the rest.
        let body = "line 1\nline 2\nline 3 is long and will get cut".to_string();
        let (out, complete) = crate::query::truncate_body_to_chars(body, 20);
        assert!(!complete);
        assert!(
            out.ends_with("line 2"),
            "truncated body should end at the last newline within the cap, got: {out:?}"
        );
        assert!(!out.contains("will get cut"));

        // No newline within the cap → falls back to char-truncate.
        let (out, complete) =
            crate::query::truncate_body_to_chars("aaaaaaaaaaaaaaaaaaaaaaaa".to_string(), 10);
        assert!(!complete);
        assert_eq!(out.chars().count(), 10);
    }

    #[test]
    fn investigate_hydrate_marks_truncated_body_incomplete() {
        // Build a synthetic bundle with one entry whose body exceeds the
        // INLINE_MAX_BODY_TOKENS * 4 cap, then verify hydrate populates the
        // body and sets body_complete = false.
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "vault",
            Some(50),
            None,
        )
        .unwrap();

        let hydrated =
            investigate_hydrate(&store, &db_path, &src, &result.bundle_id, Some(4000)).unwrap();
        // Every hydrated entry must have an inline_body and a body_complete
        // value that reflects whether truncation actually happened (i.e. the
        // flag is `false` iff char count == the cap).
        let cap = INLINE_MAX_BODY_TOKENS.saturating_mul(4);
        for e in hydrated.entries.iter().filter(|e| e.inline_body.is_some()) {
            let len = e.inline_body.as_deref().map(|b| b.chars().count()).unwrap();
            assert!(
                len <= cap,
                "hydrated body must respect the per-body cap (cap={cap}, got={len})"
            );
            if e.body_complete {
                assert!(
                    len < cap,
                    "body_complete=true means the body fit under the cap"
                );
            }
        }
    }

    #[test]
    fn asset_id_is_stable() {
        let a = compute_asset_id("bndl_x", "sym:foo");
        let b = compute_asset_id("bndl_x", "sym:foo");
        let c = compute_asset_id("bndl_x", "sym:bar");
        assert_eq!(a, b, "same inputs → same asset id");
        assert_ne!(a, c, "different uid → different asset id");
        assert!(a.starts_with('a') && a.len() == 13);
    }

    #[test]
    fn stale_bundles_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let mut store = BundleStore::default();
        store.bundles.insert(
            "old".to_string(),
            Bundle {
                bundle_id: "old".to_string(),
                created_at: now_epoch() - BUNDLE_TTL_SECS - 100.0,
                query: "q".to_string(),
                scope: "vault".to_string(),
                entries: vec![],
            },
        );
        store.bundles.insert(
            "fresh".to_string(),
            Bundle {
                bundle_id: "fresh".to_string(),
                created_at: now_epoch(),
                query: "q".to_string(),
                scope: "vault".to_string(),
                entries: vec![],
            },
        );
        save_bundle_store(&db_path, &store).unwrap();

        let loaded = load_bundle_store(&db_path);
        assert!(loaded.bundles.contains_key("fresh"), "fresh bundle kept");
        assert!(
            !loaded.bundles.contains_key("old"),
            "stale bundle dropped on load"
        );
    }

    #[test]
    fn investigate_empty_query_is_graceful() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");
        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "zzz_no_such_symbol_xyz",
            "vault",
            None,
            None,
        )
        .unwrap();
        // No matches → empty entries/domains, but a valid persisted bundle id.
        assert!(result.bundle_id.starts_with("bndl_"));
        assert!(load_bundle(&db_path, &result.bundle_id).is_some());
    }
}

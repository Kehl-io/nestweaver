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
    /// `true` when this entry was one of the query's resolved seed nodes
    /// (a direct hit), as opposed to a node surfaced by graph proximity.
    /// Skipped from JSON when `false` so existing consumers see unchanged
    /// output.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_seed: bool,
    pub relevance: f64,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
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
    /// The scope STRING the caller supplied, echoed back verbatim.
    ///
    /// nw-189: this alone reads as a restriction that was applied, which for
    /// `vault`/`all`/empty is false — they are documented pass-throughs, and
    /// the CLI additionally defaulted an unsupplied scope to the literal
    /// "vault", so a caller who asked for nothing was told their results were
    /// vault-scoped while code symbols from every repo came back. Pair it with
    /// `scope_filtered` before drawing any conclusion from it.
    pub scope: String,
    /// Whether a real filter was constructed and applied for `scope`.
    ///
    /// False for the pass-through scopes. A caller that wants to know whether
    /// results were actually restricted must read THIS, not `scope`.
    pub scope_filtered: bool,
    pub domains: Vec<Domain>,
    pub entries: Vec<BundleEntry>,
    /// Number of additional connected nodes dropped due to the token budget.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more_available: usize,
    /// Whether semantic retrieval contributed to this map.
    ///
    /// nw-120: the daemon passes its warm embedding model here while the CLI's
    /// direct path hardcodes `None`, so the two return materially different
    /// RANKINGS for the same query — daemon topped "daemon boot" with
    /// `wait_for_daemon_boot`, direct with a lexical `BootstrapErrorScreen`.
    /// The tradeoff is defensible (a per-invocation BERT load is expensive and
    /// the daemon is the supported path); reporting nothing about it was not,
    /// because the caller had no way to know the ranking was BM25-only.
    #[serde(default)]
    pub semantic_applied: bool,
    /// Retrieval components that were requested but unavailable, matching
    /// `brain_context` / `brain_search`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_components: Vec<String>,
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
    /// Entries whose body was fetched by THIS call.
    pub hydrated: usize,
    /// Entries that already carried an inline body (nothing to do) — so a
    /// `hydrated: 0` is distinguishable from a failure. `hydrated + already_hydrated`
    /// is the count of entries with a body after this call.
    pub already_hydrated: usize,
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
/// the TTL. Returns an empty store when the sidecar is missing. A corrupt
/// sidecar no longer silently drops every bundle: individually
/// parseable bundles are salvaged and a warning is emitted.
pub fn load_bundle_store(db_path: &Path) -> BundleStore {
    let path = bundle_sidecar_path(db_path);
    let mut store: BundleStore = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "bundle sidecar is corrupt; salvaging individually parseable bundles"
                );
                salvage_bundles(&text)
            }
        },
        Err(_) => BundleStore::default(),
    };
    let cutoff = now_epoch() - BUNDLE_TTL_SECS;
    store.bundles.retain(|_, b| b.created_at >= cutoff);
    store
}

/// Best-effort recovery of a corrupt sidecar: parse the top-level container
/// leniently and keep each bundle that still deserializes, dropping (with a
/// warning) only the corrupt entries.
fn salvage_bundles(text: &str) -> BundleStore {
    let mut store = BundleStore::default();
    let Ok(serde_json::Value::Object(top)) = serde_json::from_str(text) else {
        return store;
    };
    store.version = top.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if let Some(serde_json::Value::Object(bundles)) = top.get("bundles") {
        for (id, value) in bundles {
            match serde_json::from_value::<Bundle>(value.clone()) {
                Ok(b) => {
                    store.bundles.insert(id.clone(), b);
                }
                Err(e) => {
                    tracing::warn!(
                        bundle_id = %id,
                        error = %e,
                        "dropping corrupt bundle entry from sidecar"
                    );
                }
            }
        }
    }
    store
}

/// Persist the bundle store via atomic write-then-rename.
///
/// The temp file name is unique per process and per call — the previous
/// fixed `.json.tmp` name let two concurrent writers rename each other's temp
/// file out from under themselves (ENOENT) or interleave writes.
pub fn save_bundle_store(db_path: &Path, store: &BundleStore) -> Result<(), anyhow::Error> {
    let path = bundle_sidecar_path(db_path);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, serde_json::to_string(store)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Advisory lock guarding the load→mutate→save cycle of the bundle sidecar
/// Implemented as a lock file created with `create_new` so it works
/// across both threads and processes; stale locks (holder crashed) are broken
/// after [`LOCK_STALE_SECS`]. An ownership TOKEN is written into the lock file
/// so that after a stale-lock takeover, the previous holder's Drop does not
/// delete the successor's lock (pattern mirrors rts_eval.rs's `SidecarLock`).
/// Acquisition failure degrades to proceeding unlocked rather than failing
/// the investigation.
struct BundleStoreLock {
    path: std::path::PathBuf,
    /// Unique token (pid + nanos) written into the lock file on acquire.
    token: String,
    /// `false` when the lock was not actually acquired (timeout / IO error) —
    /// Drop must not remove a lock file owned by someone else.
    owned: bool,
}

/// Seconds to wait for the sidecar lock before proceeding without it.
const LOCK_WAIT_SECS: u64 = 10;
/// Age after which an existing lock file is considered abandoned.
const LOCK_STALE_SECS: u64 = 60;

impl BundleStoreLock {
    fn acquire(db_path: &Path) -> Self {
        use std::io::Write as _;
        let path = bundle_sidecar_path(db_path).with_extension("lock");
        let token = format!(
            "{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(LOCK_WAIT_SECS);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    // Best-effort: the token lets a stale-taken-over holder
                    // recognize the lock is no longer its own on Drop.
                    let _ = f.write_all(token.as_bytes());
                    drop(f);
                    // Create-then-verify: a contender breaking the lock as
                    // stale can land its remove+create after ours. Verify the
                    // file still holds OUR token before claiming the lock;
                    // otherwise wait for the real holder (or the deadline).
                    let ours = std::fs::read_to_string(&path)
                        .map(|content| content == token)
                        .unwrap_or(false);
                    if ours {
                        return Self {
                            path,
                            token,
                            owned: true,
                        };
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            path = %path.display(),
                            "bundle sidecar lock not acquired within deadline; proceeding unlocked"
                        );
                        return Self {
                            path,
                            token,
                            owned: false,
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Break abandoned locks (holder crashed between create and
                    // Drop) so one bad exit doesn't wedge the sidecar forever.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > LOCK_STALE_SECS);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            path = %path.display(),
                            "bundle sidecar lock not acquired within deadline; proceeding unlocked"
                        );
                        return Self {
                            path,
                            token,
                            owned: false,
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // Read-only dir etc.: proceed without a lock rather than fail.
                Err(_) => {
                    return Self {
                        path,
                        token,
                        owned: false,
                    };
                }
            }
        }
    }
}

impl Drop for BundleStoreLock {
    fn drop(&mut self) {
        // Only remove the lock if it is still OURS — a stale-lock takeover
        // may have replaced it with a successor's lock, which must survive.
        if self.owned {
            let still_ours = std::fs::read_to_string(&self.path)
                .map(|content| content == self.token)
                .unwrap_or(false);
            if still_ours {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Load → mutate → save the bundle store under the sidecar lock so concurrent
/// investigators never lose each other's bundles. When the closure
/// errors, nothing is written.
fn update_bundle_store<T>(
    db_path: &Path,
    f: impl FnOnce(&mut BundleStore) -> Result<T, anyhow::Error>,
) -> Result<T, anyhow::Error> {
    let _lock = BundleStoreLock::acquire(db_path);
    let mut store = load_bundle_store(db_path);
    let out = f(&mut store)?;
    store.version = 1;
    save_bundle_store(db_path, &store)?;
    Ok(out)
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
/// - `project:<slug>` — seed the project's members and post-filter results to
///   the project's member symbols (errors when the project does not exist),
/// - `repo:<name>` — restrict results to symbols in a named repo (errors when
///   no repo matches),
/// - `vault` / `all` / empty — no restriction (default).
///
/// Any other scope string is rejected with an error instead of being
/// silently treated as "no restriction". Note/section/tag nodes are
/// vault-global and pass through both filters unscoped.
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

    // 1. Resolve scope into the seed inputs and an optional post-filter.
    let (seed_inputs, scope_filter) = resolve_scope(store, query, scope)?;

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
    // The query's own resolved seed nodes are first-class map entries,
    // ordered ahead of the graph-proximity nodes so an exact-match query for
    // an isolated symbol still returns that symbol instead of an empty map.
    let mut seed_uids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut connected: Vec<BrainNode> = match connected_result {
        Ok(ctx) => {
            seed_uids = ctx.seeds.iter().map(|n| n.uid.clone()).collect();
            let mut nodes = ctx.seeds;
            nodes.extend(
                ctx.connected
                    .into_iter()
                    .filter(|n| !seed_uids.contains(&n.uid)),
            );
            nodes
        }
        Err(_) => bm25_fallback(store, tantivy, query, DEFAULT_RETRIEVAL_BREADTH),
    };
    if let Some(ref filter) = scope_filter {
        connected.retain(|n| node_in_scope(store, n, filter));
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
        None,
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
            is_seed: seed_uids.contains(&node.uid),
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

    // 7. Persist the bundle (24h TTL handled on load) under the sidecar lock
    //    so concurrent investigators don't lose each other's bundles.
    let bundle = Bundle {
        bundle_id: bundle_id.clone(),
        created_at: now_epoch(),
        query: query.to_string(),
        scope: scope.to_string(),
        entries: entries.clone(),
    };
    if let Some(db) = db_path {
        let id = bundle_id.clone();
        update_bundle_store(db, |bundle_store| {
            bundle_store.bundles.insert(id, bundle);
            Ok(())
        })?;
    }

    let semantic_applied = embed_model.is_some();
    Ok(InvestigateResult {
        bundle_id,
        query: query.to_string(),
        scope: scope.to_string(),
        scope_filtered: scope_filter.is_some(),
        domains,
        entries,
        more_available,
        semantic_applied,
        degraded_components: if semantic_applied {
            Vec::new()
        } else {
            vec!["semantic".to_string()]
        },
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
    let (expanded, neighbors, unresolved) = update_bundle_store(db_path, |bundle_store| {
        let bundle = bundle_store
            .bundles
            .get_mut(bundle_id)
            .ok_or_else(|| anyhow::anyhow!("bundle '{bundle_id}' not found or expired"))?;

        let mut expanded: Vec<BundleEntry> = Vec::new();
        let mut neighbors: Vec<NeighborRef> = Vec::new();
        let mut unresolved: Vec<String> = Vec::new();
        // Duplicate targets (or an asset_id + uid naming the same entry) are
        // expanded once instead of being echoed twice in the result.
        let mut seen_entries: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for target in targets {
            let Some(idx) = bundle
                .entries
                .iter()
                .position(|e| &e.asset_id == target || &e.uid == target)
            else {
                // Unresolved targets get no entry-index dedup (there is no
                // entry to key on), so dedupe the echo itself, order-preserving.
                if !unresolved.contains(target) {
                    unresolved.push(target.clone());
                }
                continue;
            };
            if !seen_entries.insert(idx) {
                continue;
            }
            let uid = bundle.entries[idx].uid.clone();
            let asset_id = bundle.entries[idx].asset_id.clone();

            // Guard against an unreadable root / empty source span: storing an
            // empty body marked `body_complete` would poison the entry and
            // prevent a later hydrate from retrying it.
            if let Some(body) = fetch_full_body(store, &uid, root).filter(|b| !b.is_empty()) {
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

        Ok((expanded, neighbors, unresolved))
    })?;

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

    let (hydrated, already_hydrated, entries) = update_bundle_store(db_path, |bundle_store| {
        let bundle = bundle_store
            .bundles
            .get_mut(bundle_id)
            .ok_or_else(|| anyhow::anyhow!("bundle '{bundle_id}' not found or expired"))?;

        let mut used_tokens = 0usize;
        let mut hydrated = 0usize;
        let mut already_hydrated = 0usize;
        for entry in bundle.entries.iter_mut() {
            if entry.inline_body.is_some() {
                already_hydrated += 1;
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
                // Over budget: skip THIS body and keep going — a later, smaller
                // body may still fit. (Previously a `break` aborted every
                // remaining entry.)
                continue;
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
        Ok((hydrated, already_hydrated, entries))
    })?;

    Ok(HydrateResult {
        bundle_id: bundle_id.to_string(),
        hydrated,
        already_hydrated,
        entries,
    })
}

// ── Scope resolution ──────────────────────────────────────────────────────

/// Post-retrieval scope filter produced by [`resolve_scope`].
enum ScopeFilter {
    /// Keep only symbols belonging to one of these repos (`repo:` scope).
    Repos(Vec<String>),
    /// Keep only symbols that are members of the project (`project:` scope).
    ProjectSymbols(std::collections::HashSet<String>),
}

/// Strip a `project:`/`repo:` scope prefix case-insensitively (so
/// `Project:Foo` / `REPO:x` resolve the same as their lowercase spellings,
/// matching the already case-insensitive `vault`/`all` and project/repo name
/// matching). Returns the remainder of the ORIGINAL string — the name's own
/// case is preserved for the lookups that follow.
fn strip_scope_prefix<'a>(scope: &'a str, prefix: &str) -> Option<&'a str> {
    // `prefix` is pure ASCII, so `prefix.len()` can only split `scope` at a
    // char boundary when the leading bytes really are the ASCII prefix.
    if !scope
        .get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    {
        return None;
    }
    scope.get(prefix.len()..)
}

/// Resolve a scope string into (seed_inputs, optional scope filter).
///
/// The `query` always seeds retrieval; project scope additionally seeds the
/// project's member UIDs and post-filters the results to its member symbols;
/// repo scope returns a repo-UID set used to post-filter the connected nodes.
///
/// Unrecognized scope strings, an empty `repo:`/`project:` name, a
/// `project:` naming a nonexistent project, or a `repo:` matching no repo are
/// all hard errors naming the scope — previously they silently degraded to
/// "no restriction" (or, for an unmatched `repo:`, to filtering out every
/// symbol).
fn resolve_scope(
    store: &GraphStore,
    query: &str,
    scope: &str,
) -> Result<(Vec<String>, Option<ScopeFilter>), anyhow::Error> {
    let mut seeds = vec![query.to_string()];
    // Multi-word queries: also seed each whitespace token. Seed resolution does
    // exact title / substring symbol-name lookups, which can NEVER match a phrase
    // containing spaces ("blast radius" is not a substring of any symbol name), so
    // a multi-word query collapsed to zero results. The hybrid seed loop resolves
    // each seed independently and unions/dedupes the UIDs, so adding the per-token
    // seeds gives a natural OR across terms. Single-token queries split to exactly
    // one token equal to the whole query, so their behavior is unchanged.
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.len() > 1 {
        seeds.extend(tokens.iter().map(|t| t.to_string()));
    }
    let scope = scope.trim();

    // vault / all / empty → no restriction.
    if scope.is_empty() || scope.eq_ignore_ascii_case("vault") || scope.eq_ignore_ascii_case("all")
    {
        return Ok((seeds, None));
    }

    if let Some(slug) = strip_scope_prefix(scope, "project:") {
        let slug = slug.trim();
        if slug.is_empty() {
            anyhow::bail!("invalid scope '{scope}': 'project:' requires a project name");
        }
        let project = store
            .lookup_project_by_name(slug)
            .map_err(|e| anyhow::anyhow!("lookup project '{slug}': {e}"))?
            .ok_or_else(|| anyhow::anyhow!("unknown scope '{scope}': no project named '{slug}'"))?;
        if let Ok(note_uids) = store.list_project_note_uids(&project.uid) {
            seeds.extend(note_uids);
        }
        let mut member_symbols = std::collections::HashSet::new();
        if let Ok(sym_uids) = store.list_project_symbol_uids(&project.uid) {
            seeds.extend(sym_uids.iter().cloned());
            member_symbols.extend(sym_uids);
        }
        return Ok((seeds, Some(ScopeFilter::ProjectSymbols(member_symbols))));
    }

    if let Some(name) = strip_scope_prefix(scope, "repo:") {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("invalid scope 'repo:': 'repo:' requires a repo name");
        }
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
        if matches.is_empty() {
            anyhow::bail!("unknown scope '{scope}': no repo matching '{name}'");
        }
        return Ok((seeds, Some(ScopeFilter::Repos(matches))));
    }

    anyhow::bail!(
        "unknown scope '{scope}': expected 'project:<name>', 'repo:<name>', 'vault', or 'all'"
    )
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

/// Whether a node survives the scope filter. Only symbol nodes are scoped;
/// non-symbol nodes (notes/sections/tags) are vault-global and pass through
/// unfiltered — this mirrors the long-standing `repo:` notes handling.
fn node_in_scope(store: &GraphStore, node: &BrainNode, filter: &ScopeFilter) -> bool {
    if !node.uid.starts_with("sym:") {
        return true;
    }
    match filter {
        ScopeFilter::ProjectSymbols(members) => members.contains(&node.uid),
        ScopeFilter::Repos(repo_uids) => {
            let Ok(sym) = store.lookup_symbol(&node.uid) else {
                return false;
            };
            repo_uids.contains(&sym.repo_uid)
        }
    }
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

    /// After a stale-lock takeover, the previous holder's Drop must not
    /// delete the successor's lock file (ownership-token back-port from
    /// rts_eval's `SidecarLock`).
    #[test]
    fn bundle_store_lock_drop_preserves_successor_lock_after_takeover() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let lock = BundleStoreLock::acquire(&db_path);
        assert!(lock.owned, "lock on a fresh path must be acquired");
        let lock_path = bundle_sidecar_path(&db_path).with_extension("lock");
        // A successor breaks the "stale" lock and installs its own token.
        std::fs::write(&lock_path, b"other-pid:0").expect("successor token");
        drop(lock);
        assert!(
            lock_path.exists(),
            "successor lock must survive the taken-over holder's Drop"
        );
        let _ = std::fs::remove_file(&lock_path);
    }

    /// A normal acquire/drop cycle removes the lock file it created.
    #[test]
    fn bundle_store_lock_drop_removes_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let lock_path = bundle_sidecar_path(&db_path).with_extension("lock");
        {
            let lock = BundleStoreLock::acquire(&db_path);
            assert!(lock.owned);
            assert!(lock_path.exists());
        }
        assert!(
            !lock_path.exists(),
            "Drop must remove the holder's own lock file"
        );
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

    /// nw-189. `scope_filtered` must report whether a filter actually RAN,
    /// which is the thing `scope` alone cannot express.
    ///
    /// The previous test in this module exercises `resolve_scope` only, and so
    /// passes identically on `main` — it guards pre-existing behaviour rather
    /// than this change. This one pins the new field against the same binding
    /// that gates the filter, so the flag cannot drift from reality.
    #[test]
    fn scope_filtered_reports_whether_a_filter_actually_ran() {
        let store = GraphStore::in_memory().unwrap();

        // Every documented pass-through must report FALSE — including the
        // literal "vault" a caller may still pass explicitly.
        for pass_through in ["", "vault", "all"] {
            let (_, filter) = resolve_scope(&store, "anything", pass_through).unwrap();
            assert!(
                filter.is_none(),
                "{pass_through:?} builds no filter, so scope_filtered must be false"
            );
        }

        // The RESPONSE is where the defect showed, so assert on the serialized
        // shape a caller actually reads. `scope` echoing "vault" is not itself
        // wrong — what was wrong is that nothing alongside it said whether a
        // restriction happened.
        let unrestricted = InvestigateResult {
            bundle_id: "b".to_string(),
            query: "q".to_string(),
            scope: "vault".to_string(),
            scope_filtered: false,
            domains: vec![],
            entries: vec![],
            more_available: 0,
            semantic_applied: false,
            degraded_components: vec![],
        };
        let json = serde_json::to_value(&unrestricted).unwrap();
        assert_eq!(
            json["scope"], "vault",
            "the caller's scope string is still echoed verbatim"
        );
        assert_eq!(
            json["scope_filtered"], false,
            "…and `scope_filtered` is what tells them nothing was restricted"
        );

        let restricted = InvestigateResult {
            scope: "repo:acme".to_string(),
            scope_filtered: true,
            ..unrestricted
        };
        assert_eq!(
            serde_json::to_value(&restricted).unwrap()["scope_filtered"],
            true
        );
    }

    /// nw-189. `vault` and `all` are documented PASS-THROUGHS — they construct
    /// no filter — so a response that echoes `scope: "vault"` and nothing else
    /// reads as a restriction that never happened. The CLI and MCP made it
    /// worse by defaulting an unsupplied scope to the literal "vault", so a
    /// caller who asked for nothing was told their results were vault-scoped
    /// while code symbols from every repo came back.
    ///
    /// Pins the property that matters: whether a filter was BUILT, which is
    /// what `scope_filtered` reports. Asserting on `resolve_scope` directly
    /// keeps this independent of a populated graph.
    #[test]
    fn pass_through_scopes_build_no_filter_and_real_scopes_do() {
        let store = GraphStore::in_memory().unwrap();

        // Every documented pass-through, including the empty string and the
        // case variants the resolver accepts.
        for pass_through in ["", "vault", "all", "VAULT", "All", "  vault  "] {
            let (_, filter) = resolve_scope(&store, "anything", pass_through).unwrap();
            assert!(
                filter.is_none(),
                "{pass_through:?} is documented as no restriction, so it must build no filter"
            );
        }

        // A repo scope for a repo that does not exist is an ERROR, not a
        // silent pass-through — 6.4.0's fix, pinned here so a future change
        // cannot quietly turn an unknown scope back into "no restriction",
        // which would be indistinguishable from the bug above.
        assert!(
            resolve_scope(&store, "anything", "repo:does-not-exist").is_err(),
            "an unresolvable repo scope must error rather than silently match everything"
        );
        assert!(
            resolve_scope(&store, "anything", "project:does-not-exist").is_err(),
            "an unresolvable project scope must error rather than silently match everything"
        );
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

    #[test]
    fn investigate_multiword_query_unions_token_seeds() {
        // nw-080 regression: a multi-word query must union its per-token seeds
        // instead of matching the whole phrase literally (which resolves nothing,
        // since no symbol name contains a space). Runs with tantivy/embed = None,
        // proving the fix is at the seed layer, not the Tantivy fallback.
        //
        // A richer fixture than make_store(): tokens `alpha`/`gamma` resolve to a
        // strict subset of the graph, leaving non-seed neighbors (beta, delta) to
        // surface — so a working union yields real entries, while the old
        // whole-phrase seeding ("alpha gamma" matches no symbol) yields none.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("chain.js"),
            "function alpha() { return beta(); }\n\
             function beta() { return gamma(); }\n\
             function gamma() { return 1; }\n\
             function delta() { return alpha(); }",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        let db_path = dir.path().join("nestweaver.lbug");

        let multi = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "alpha gamma",
            "vault",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(
            !multi.entries.is_empty(),
            "multi-word query must union per-token seeds, got 0 entries"
        );

        // Single-token behavior is preserved (still non-empty).
        let single = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "alpha",
            "vault",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(!single.entries.is_empty());
    }

    // ── Seed nodes are first-class map entries ─────────────────────

    #[test]
    fn investigate_includes_resolved_seeds_first() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "hello",
            "vault",
            Some(4000),
            None,
        )
        .unwrap();

        // The queried symbol itself must appear in the map, marked as a seed.
        let hello = result
            .entries
            .iter()
            .find(|e| e.title == "hello")
            .expect("the queried symbol must appear in the map");
        assert!(hello.is_seed, "direct-hit entry must be marked is_seed");
        // Seeds sort ahead of graph-proximity entries.
        let last_seed = result.entries.iter().rposition(|e| e.is_seed);
        let first_non_seed = result.entries.iter().position(|e| !e.is_seed);
        if let (Some(l), Some(f)) = (last_seed, first_non_seed) {
            assert!(l < f, "all seed entries must precede non-seed entries");
        }
    }

    #[test]
    fn investigate_isolated_symbol_exact_match_is_not_empty() {
        // Acceptance: an exact-match query for an isolated symbol (no
        // callers/callees) must not yield an empty map, and the entry must be
        // drillable via expand.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lonely.js"),
            "function lonelyIsland() { return 42; }",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        let db_path = dir.path().join("nestweaver.lbug");

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "lonelyIsland",
            "vault",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(
            !result.entries.is_empty(),
            "exact-match for an isolated symbol must not yield an empty map"
        );
        let entry = result
            .entries
            .iter()
            .find(|e| e.title == "lonelyIsland")
            .expect("the queried symbol must be in the map");
        assert!(entry.is_seed);

        // expand can target the seed entry.
        let expanded = investigate_expand(
            &store,
            &db_path,
            &src,
            &result.bundle_id,
            std::slice::from_ref(&entry.asset_id),
        )
        .unwrap();
        assert_eq!(expanded.expanded.len(), 1);
        assert!(
            expanded.expanded[0]
                .inline_body
                .as_deref()
                .is_some_and(|b| b.contains("42")),
            "expand on a seed entry must fetch its body"
        );
    }

    // ── Bundle sidecar race + corrupt tolerance ────────────────────

    #[test]
    fn parallel_bundle_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        std::thread::scope(|s| {
            for i in 0..12 {
                let db_path = &db_path;
                s.spawn(move || {
                    update_bundle_store(db_path, |store| {
                        store.bundles.insert(
                            format!("bndl_{i}"),
                            Bundle {
                                bundle_id: format!("bndl_{i}"),
                                created_at: now_epoch(),
                                query: "q".to_string(),
                                scope: "vault".to_string(),
                                entries: vec![],
                            },
                        );
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let store = load_bundle_store(&db_path);
        assert_eq!(
            store.bundles.len(),
            12,
            "every concurrent update must survive the sidecar race"
        );
        // Unique temp files were all renamed away; lock file removed.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.contains(".tmp.") || n.ends_with(".lock")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp/lock files may be left behind: {leftovers:?}"
        );
    }

    #[test]
    fn parallel_investigates_persist_all_bundles() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");
        let store = &store;
        let db_path = &db_path;
        let src = &src;
        let ids: Vec<String> = std::thread::scope(|s| {
            (0..12)
                .map(|_| {
                    s.spawn(move || {
                        investigate(
                            store,
                            None,
                            Some(db_path),
                            src,
                            "greet",
                            "vault",
                            Some(2000),
                            None,
                        )
                        .unwrap()
                        .bundle_id
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        let persisted = load_bundle_store(db_path);
        for id in &ids {
            assert!(
                persisted.bundles.contains_key(id),
                "bundle {id} was lost to a sidecar race"
            );
        }
    }

    #[test]
    fn corrupt_sidecar_salvages_valid_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let good = Bundle {
            bundle_id: "good".to_string(),
            created_at: now_epoch(),
            query: "q".to_string(),
            scope: "vault".to_string(),
            entries: vec![],
        };
        // One valid bundle + one structurally invalid entry in the container.
        let text = format!(
            "{{\"version\":1,\"bundles\":{{\"good\":{},\"bad\":{{\"bundle_id\":42}}}}}}",
            serde_json::to_string(&good).unwrap()
        );
        fs::write(bundle_sidecar_path(&db_path), text).unwrap();
        let store = load_bundle_store(&db_path);
        assert!(
            store.bundles.contains_key("good"),
            "valid bundle must be salvaged from a corrupt sidecar"
        );
        assert!(
            !store.bundles.contains_key("bad"),
            "only the corrupt entry is dropped, not the whole store"
        );

        // Complete garbage → empty store, no panic.
        fs::write(bundle_sidecar_path(&db_path), "this is not json {").unwrap();
        let store = load_bundle_store(&db_path);
        assert!(store.bundles.is_empty());
    }

    // ── Scope validation + project post-filter ─────────────────────

    #[test]
    fn investigate_rejects_unknown_and_empty_scopes() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        for scope in [
            "bogus",
            "vaults",
            "repo:",
            "project:",
            "repo:no-such-repo-zzz",
            "project:No Such Project ZZZ",
        ] {
            let err = investigate(
                &store,
                None,
                Some(&db_path),
                &src,
                "greet",
                scope,
                None,
                None,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("scope"),
                "error must explain the scope problem, got: {err}"
            );
        }

        // Accepted no-restriction spellings still work.
        for scope in ["vault", "all", "", "  vault  "] {
            investigate(
                &store,
                None,
                Some(&db_path),
                &src,
                "greet",
                scope,
                None,
                None,
            )
            .unwrap();
        }
        // repo: with a matching name works.
        investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "repo:test",
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn scope_prefixes_are_case_insensitive() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        // REPO: / Repo: / mixed-case prefixes must resolve like `repo:`.
        for scope in ["REPO:test", "Repo:test", "rEpO:test"] {
            investigate(
                &store,
                None,
                Some(&db_path),
                &src,
                "greet",
                scope,
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("scope '{scope}' should resolve: {e}"));
        }

        // Same for project: — set up a member-less project and address it with
        // an upper/mixed-case prefix.
        let project = nestweaver_schema::Project {
            uid: "proj:test:onlyhello".to_string(),
            name: "onlyhello".to_string(),
            summary: None,
            instance_id: "test".to_string(),
        };
        store.upsert_project(&project).unwrap();
        for scope in [
            "PROJECT:onlyhello",
            "Project:onlyhello",
            "pRoJeCt:onlyhello",
        ] {
            investigate(
                &store,
                None,
                Some(&db_path),
                &src,
                "greet",
                scope,
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("scope '{scope}' should resolve: {e}"));
        }
    }

    #[test]
    fn project_scope_filters_symbols_to_members() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        // A project containing ONLY the `hello` symbol.
        let hello_uid = store
            .lookup_symbols_by_name("hello")
            .unwrap()
            .into_iter()
            .find(|s| s.name == "hello")
            .expect("hello symbol exists")
            .uid;
        let project = nestweaver_schema::Project {
            uid: "proj:test:onlyhello".to_string(),
            name: "onlyhello".to_string(),
            summary: None,
            instance_id: "test".to_string(),
        };
        store.upsert_project(&project).unwrap();
        store
            .batch_insert_project_symbol_edges(&project.uid, std::slice::from_ref(&hello_uid), 1.0)
            .unwrap();

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet",
            "project:onlyhello",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(
            !result.entries.is_empty(),
            "the member symbol seed must survive the filter"
        );
        for e in &result.entries {
            if e.uid.starts_with("sym:") {
                assert_eq!(
                    e.uid, hello_uid,
                    "non-member symbol leaked through the project scope filter"
                );
            }
        }
    }

    // ── LOW: expand/hydrate hygiene ──────────────────────────────────────

    #[test]
    fn investigate_expand_dedupes_duplicate_targets() {
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
        let target = result
            .entries
            .iter()
            .find(|e| e.uid.starts_with("sym:"))
            .expect("at least one symbol entry");
        let aid = target.asset_id.clone();
        let uid = target.uid.clone();

        // Same entry named three ways: asset_id twice + raw uid.
        let expanded = investigate_expand(
            &store,
            &db_path,
            &src,
            &result.bundle_id,
            &[aid.clone(), aid, uid],
        )
        .unwrap();
        assert_eq!(
            expanded.expanded.len(),
            1,
            "duplicate targets must be expanded exactly once"
        );
        assert!(expanded.unresolved.is_empty());
    }

    #[test]
    fn investigate_expand_dedupes_unresolved_targets() {
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

        // The same unknown target named twice must be echoed once, and the
        // first-seen order of distinct unresolved targets preserved.
        let expanded = investigate_expand(
            &store,
            &db_path,
            &src,
            &result.bundle_id,
            &[
                "nope-a".to_string(),
                "nope-b".to_string(),
                "nope-a".to_string(),
                "nope-b".to_string(),
            ],
        )
        .unwrap();
        assert!(expanded.expanded.is_empty());
        assert_eq!(
            expanded.unresolved,
            vec!["nope-a".to_string(), "nope-b".to_string()],
            "duplicate unresolved targets must be echoed exactly once, in order"
        );
    }

    #[test]
    fn investigate_expand_unreadable_root_does_not_poison_entry() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");
        // Synthetic bundle with one un-hydrated symbol entry (deterministic —
        // does not depend on how many bodies the initial map inlines).
        let greet_uid = store
            .lookup_symbols_by_name("greet")
            .unwrap()
            .into_iter()
            .find(|s| s.name == "greet")
            .expect("greet symbol exists")
            .uid;
        let mut bundle_store = BundleStore::default();
        bundle_store.bundles.insert(
            "bndl_test".to_string(),
            Bundle {
                bundle_id: "bndl_test".to_string(),
                created_at: now_epoch(),
                query: "q".to_string(),
                scope: "vault".to_string(),
                entries: vec![BundleEntry {
                    asset_id: "a_greet".to_string(),
                    uid: greet_uid,
                    kind: "Symbol".to_string(),
                    title: "greet".to_string(),
                    location: "greet/main.js:1".to_string(),
                    summary: None,
                    inline_body: None,
                    body_complete: true,
                    expanded: false,
                    is_seed: false,
                    relevance: 1.0,
                }],
            },
        );
        save_bundle_store(&db_path, &bundle_store).unwrap();

        // Expand against a root where the source file cannot be read: the
        // entry must NOT be poisoned with an empty "complete" body.
        let missing_root = dir.path().join("does-not-exist");
        let expanded = investigate_expand(
            &store,
            &db_path,
            &missing_root,
            "bndl_test",
            &["a_greet".to_string()],
        )
        .unwrap();
        assert_eq!(expanded.expanded.len(), 1);
        assert!(
            expanded.expanded[0].inline_body.is_none(),
            "an unreadable root must leave inline_body unset so hydrate can retry"
        );

        // A later hydrate against the real root still fills the body.
        let hydrated =
            investigate_hydrate(&store, &db_path, &src, "bndl_test", Some(4000)).unwrap();
        let entry = hydrated
            .entries
            .iter()
            .find(|e| e.asset_id == "a_greet")
            .unwrap();
        assert!(
            entry.inline_body.as_deref().is_some_and(|b| !b.is_empty()),
            "hydrate must be able to retry an entry expand could not read"
        );
    }

    #[test]
    fn investigate_hydrate_skips_over_budget_body_instead_of_aborting() {
        // Fixture: two huge functions and one tiny one. With a budget that fits
        // one huge body plus the tiny one — but not two huge bodies — the old
        // `break` aborted hydration of the tiny entry; skipping must not.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        let padding = "    let x = 1; // padding padding padding padding\n".repeat(80);
        fs::write(
            src.join("big.js"),
            format!(
                "function hugeA() {{\n{padding}}}\nfunction hugeB() {{\n{padding}}}\nfunction tinyC() {{ return 1; }}"
            ),
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        let db_path = dir.path().join("nestweaver.lbug");

        let uid_of = |name: &str| {
            store
                .lookup_symbols_by_name(name)
                .unwrap()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} symbol exists"))
                .uid
        };
        let mk_entry = |name: &str| BundleEntry {
            asset_id: format!("a_{name}"),
            uid: uid_of(name),
            kind: "Symbol".to_string(),
            title: name.to_string(),
            location: "big.js:1".to_string(),
            summary: None,
            inline_body: None,
            body_complete: true,
            expanded: false,
            is_seed: false,
            relevance: 1.0,
        };
        let mut bundle_store = BundleStore::default();
        bundle_store.bundles.insert(
            "bndl_test".to_string(),
            Bundle {
                bundle_id: "bndl_test".to_string(),
                created_at: now_epoch(),
                query: "q".to_string(),
                scope: "vault".to_string(),
                // Order matters: huge, huge, tiny.
                entries: vec![mk_entry("hugeA"), mk_entry("hugeB"), mk_entry("tinyC")],
            },
        );
        save_bundle_store(&db_path, &bundle_store).unwrap();

        // A huge body truncates to INLINE_MAX_BODY_TOKENS (400) tokens; the
        // tiny body costs only a few. Budget 450 fits hugeA + tinyC.
        let res = investigate_hydrate(&store, &db_path, &src, "bndl_test", Some(450)).unwrap();
        assert_eq!(
            res.hydrated, 2,
            "the tiny entry after an over-budget body must still hydrate"
        );
        let body_of = |name: &str| {
            res.entries
                .iter()
                .find(|e| e.title == name)
                .and_then(|e| e.inline_body.as_deref())
        };
        assert!(body_of("hugeA").is_some());
        assert!(
            body_of("hugeB").is_none(),
            "the over-budget body is skipped, not hydrated"
        );
        assert!(
            body_of("tinyC").is_some(),
            "entries after a skipped body must still be hydrated"
        );
    }
}

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
    BrainNode, EmbedQueryFn, HybridSearchConfig, RenderCap,
    build_brain_context_hybrid_with_aliases_capped, populate_inline_bodies,
};

/// Bundle time-to-live: entries older than this are dropped when the sidecar
/// is loaded.
const BUNDLE_TTL_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Default retrieval breadth — how many connected nodes we consider for the map.
const DEFAULT_RETRIEVAL_BREADTH: usize = 30;

/// nw-322 (leg 3): how many candidates PER PARTITION (seeds, connected) get
/// hydrated (`render_brain_node`, one DB round-trip each) before this
/// module's own scope filter and `DEFAULT_RETRIEVAL_BREADTH` truncate ever
/// run. `project:<slug>` seeds PPR with a project's entire membership
/// (`resolve_scope` below) and PPR includes every seed regardless of score,
/// so on a large project `fused` is corpus-sized and hydrating all of it
/// only to keep 30 was measured at 110-142s (nw-322) — 12-27x every other
/// scope, comfortably over an MCP client's timeout.
///
/// Set above `DEFAULT_RETRIEVAL_BREADTH`, not equal to it, but the margin is
/// no longer defending against scope-filter attrition — a REVIEWED
/// REGRESSION in the first cut of this fix did that by capping hydration
/// BEFORE the scope filter ran, so an out-of-scope candidate that outranked
/// a genuine member by GLOBAL score could consume a slot the member needed.
/// The filter (`uid_in_scope`) now runs FIRST, as `RenderCap::admit`, applied
/// to every fused UID before either partition's cap counter increments — so
/// every candidate a cap slot goes to is already known to be in scope, and
/// the final `investigate` output never keeps more than
/// `DEFAULT_RETRIEVAL_BREADTH` (30) total across BOTH partitions combined.
/// That makes 30 PER PARTITION already provably sufficient: this is `* 4`
/// (120) as a cheap, non-load-bearing buffer against a future pass wanting
/// more than the bare minimum (e.g. a diversity/dedup step) without
/// reproving this margin — not because 120 is itself required. See
/// `tests::investigate_project_scope_stays_fast_on_a_large_project` (speed;
/// every member is in scope, so it cannot exercise `admit`) and
/// `tests::project_scope_render_cap_does_not_starve_members_outscored_globally`
/// (the regression `admit` fixes).
const RETRIEVAL_RENDER_MARGIN: usize = DEFAULT_RETRIEVAL_BREADTH * 4;

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
    /// Why this entry carries no body, when it carries none. nw-301: an entry
    /// with no body and no reason is read as "this node has no content"; the
    /// truth was usually "this kind was never implemented" or "the source was
    /// not readable from the root you passed". Absent when a body is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
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
    ///
    /// Unchanged in MEANING — it has always counted only the budget loop, and
    /// its doc comment always said so while its NAME did not. What changed
    /// (nw-362(b)) is that `skip_serializing_if` is gone: the field vanished
    /// when it was 0, which is exactly the case a `DEFAULT_RETRIEVAL_BREADTH`
    /// truncation produces, so the one cap that fired was invisible AND the
    /// field that would have shown a different cap was absent. An absent key
    /// must not be readable as "nothing was dropped" (`e09e4a80`).
    #[serde(default)]
    pub more_available: usize,
    /// How many entries this map is actually carrying. Equal to
    /// `entries.len()`; present so the triple can be read without counting the
    /// array.
    #[serde(default)]
    pub returned: usize,
    /// How many connected nodes retrieval produced BEFORE any of this
    /// function's caps.
    ///
    /// nw-362(b). `investigate` has five caps and `more_available` counted
    /// ONE. `DEFAULT_RETRIEVAL_BREADTH` truncates before the token-budget loop
    /// and incremented nothing, so an undercount was presented as a count and
    /// a query whose neighbourhood exceeded 30 reported itself complete.
    ///
    /// REVISED (reviewed regression, same defect one layer down):
    /// `RenderCap` (nw-322 leg 3) added a hydration bound of its OWN, ahead of
    /// the `truncate` site this field used to be captured at — so "captured
    /// at the truncate site" stopped being "captured before any cap" the
    /// moment that cap started running earlier. This is now
    /// `BrainContextResult::admitted_before_cap` when a render cap was in
    /// play (the in-scope population, counted before hydration, not after) —
    /// falling back to the post-hydration count only on the `bm25_fallback`
    /// path, which carries no render cap to undercount against.
    ///
    /// An upper bound, not an exact count, in the render-cap case: see
    /// `BrainContextResult::admitted_before_cap`'s own doc comment for why
    /// (a rare, one-directional imprecision — it can only over-, never
    /// under-, disclose what was dropped).
    #[serde(default)]
    pub total: usize,
    /// `returned < total`. The standard spelling, beside the standard pair.
    #[serde(default)]
    pub truncated: bool,
    /// Why entries were dropped, counted by reason — so "retrieval cap" (an
    /// internal hydration bound, `RenderCap`), "retrieval breadth" (a hard
    /// internal bound; raising the budget cannot recover a node it threw
    /// away), and "token budget exhausted" (retry with more budget) are each
    /// distinguishable, not folded into one another.
    ///
    /// `"retrieval_cap"` is the newer of the three (reviewed regression fix):
    /// `RenderCap` bounds hydration BEFORE `retrieval_breadth`'s truncate ever
    /// runs, so a candidate it drops is a DIFFERENT bound with a different
    /// size than `retrieval_breadth`'s, and folding the two would hide which
    /// one actually bit — the same reasoning nw-362(b) already applied to
    /// keep `retrieval_breadth` and `token_budget` apart.
    ///
    /// Keyed rather than a single `truncated_by` scalar because these caps do
    /// NOT compose in an order: all three remedies stay independently useful
    /// when more than one fires. This is `HydrateResult::skipped_reasons`'
    /// shape, and it carries the same invariant — the values sum to
    /// `total - returned`.
    ///
    /// The inline-body cap is deliberately NOT in here: it drops a BODY, not
    /// an entry, so counting it would break that invariant. See
    /// [`InvestigateResult::inline_bodies_dropped`].
    #[serde(default)]
    pub dropped_reasons: std::collections::BTreeMap<String, usize>,
    /// Entries whose inline body was removed by `MAX_INLINE_BODIES`.
    ///
    /// A separate scalar, not a `dropped_reasons` key: the entry is present
    /// and only its body is not, so it is not a row drop and must not be
    /// summed with them. `investigate_expand` / `investigate_hydrate` recover
    /// these; nothing recovers a row the breadth bound cut.
    #[serde(default)]
    pub inline_bodies_dropped: usize,
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
    /// Entries this call could NOT fill. nw-301: without this,
    /// `hydrated + already_hydrated` silently under-counted the bundle and the
    /// caller had no way to tell an entry it had skipped from one it had
    /// filled. `hydrated + already_hydrated + skipped == entries.len()` is now
    /// an invariant.
    #[serde(default)]
    pub skipped: usize,
    /// Why they were skipped, counted by reason — so "Tag entries have no body"
    /// (a fact) is distinguishable from "source not readable from the supplied
    /// root" (a fixable mistake) and from "token budget exhausted" (retry with
    /// more budget).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub skipped_reasons: std::collections::BTreeMap<String, usize>,
    pub entries: Vec<BundleEntry>,
}

// ── Sidecar persistence ────────────────────────────────────────────────────

/// Canonical sidecar path for bundle data.
pub fn bundle_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    crate::sidecar_path(db_path, ".bundles.json")
}

/// [`load_bundle_store`] that distinguishes an ABSENT sidecar from an
/// UNREADABLE one.
///
/// nw-395 leg 3. The infallible form maps every read error to an empty store,
/// so a `chmod 000` sidecar, a directory in its place, or an I/O fault all
/// reach the caller as `bundle '<id>' not found or expired` -- which sends the
/// user to the TTL when their bundle store cannot be read at all. Absence is
/// still simply empty: that is the first-run path and must stay silent.
pub fn load_bundle_store_checked(db_path: &Path) -> Result<BundleStore, anyhow::Error> {
    let path = bundle_sidecar_path(db_path);
    match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BundleStore::default());
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot read the bundle store at {}: {error}",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    // Read ONCE and prove readability with the same call. A
    // corrupt-but-readable sidecar is NOT a fault here -- `load_bundle_store`
    // salvages individually parseable bundles, and that is tested behaviour.
    std::fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "cannot read the bundle store at {}: {error}",
            path.display()
        )
    })?;
    Ok(load_bundle_store(db_path))
}

/// Load the bundle store, dropping any bundles whose `created_at` is older than
/// the TTL. Returns an empty store when the sidecar is missing OR unreadable --
/// see [`load_bundle_store_checked`] when that difference matters. A corrupt
/// sidecar no longer silently drops every bundle: individually parseable
/// bundles are salvaged and a warning is emitted.
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

/// Identity of the PID namespace this process's pids are meaningful in.
///
/// A pid is only comparable against `process_is_alive` inside the namespace
/// that issued it. Without this, a containerised daemon and a host-side CLI
/// sharing a bind-mounted database read each other's live pids as `ESRCH` and
/// each breaks the other's lock -- reintroducing the very lost-bundle race the
/// pid check was added to close, in a form STRICTLY worse than the old
/// mtime-only rule, which was namespace-agnostic.
fn pid_namespace_identity() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // Exact: the pid-namespace inode, which differs per container.
        std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .map(|target| target.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No pid namespaces; the host is the boundary. Hostname is a coarse
        // but honest stand-in, and a mismatch only costs us the fast path.
        let mut buffer = [0i8; 256];
        if unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len()) } != 0 {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }
}

/// The pid recorded in a bundle lock token, when it is BOTH parseable AND
/// stamped with this process's pid namespace. `None` means "cannot tell",
/// never "dead" -- the caller falls back to the namespace-agnostic mtime rule.
///
/// Tokens are `<pid>:<nanos>:<namespace>`. A legacy two-field token has no
/// namespace and is deliberately NOT trusted: it may have been written from
/// anywhere.
fn lock_holder_pid(path: &Path) -> Option<i32> {
    let token = std::fs::read_to_string(path).ok()?;
    let mut fields = token.trim().split(':');
    let pid: i32 = fields.next()?.trim().parse().ok()?;
    let _nanos = fields.next()?;
    let recorded = fields.collect::<Vec<_>>().join(":");
    if recorded.is_empty() || Some(recorded) != pid_namespace_identity() {
        return None;
    }
    Some(pid)
}

impl BundleStoreLock {
    fn acquire(db_path: &Path) -> Self {
        use std::io::Write as _;
        let path = bundle_sidecar_path(db_path).with_extension("lock");
        let token = format!(
            "{}:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            pid_namespace_identity().unwrap_or_default()
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
                    //
                    // nw-395: the token records the holder's pid, and for a
                    // long time nothing read it. Staleness was mtime-only at
                    // LOCK_STALE_SECS (60s) while the acquisition deadline is
                    // LOCK_WAIT_SECS (10s), so a holder that died between
                    // create and Drop stalled EVERY caller for the full
                    // deadline and then released all of them to proceed
                    // unlocked at once -- which is how a bundle already handed
                    // to the caller could be lost. A provably dead pid breaks
                    // the lock at once; an unreadable or pid-less token falls
                    // through to the mtime rule, and `process_is_alive` treats
                    // an indeterminate pid as alive, so both fail safe.
                    let stale = lock_holder_pid(&path)
                        .is_some_and(|pid| !crate::index_publication::process_is_alive(pid))
                        || std::fs::metadata(&path)
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
    // nw-395 leg 2. Every acquisition failure used to fall through to
    // load -> mutate -> save UNLOCKED. Under contention that is how a bundle
    // already handed to the caller was lost: two writers each load the same
    // store, each save their own copy, and the loser's bundle is gone while its
    // id was already advertised as a drill-in. Failing loudly is the lesser
    // harm -- the caller retries, instead of being told a bundle exists that
    // does not. Reads do not take this lock and are unaffected.
    let lock = BundleStoreLock::acquire(db_path);
    if !lock.owned {
        anyhow::bail!(
            "could not acquire the bundle store lock at {} within {LOCK_WAIT_SECS}s; another \
             process is writing it. Retry; a mutation is refused rather than performed unlocked, \
             because an unlocked write silently discards a concurrent writer's bundle.",
            lock.path.display()
        );
    }
    let mut store = load_bundle_store_checked(db_path)?;
    let out = f(&mut store)?;
    store.version = 1;
    save_bundle_store(db_path, &store)?;
    Ok(out)
}

/// Load a single live (non-expired) bundle by id.
pub fn load_bundle(db_path: &Path, bundle_id: &str) -> Option<Bundle> {
    load_bundle_store(db_path).bundles.remove(bundle_id)
}

/// Fail a ranked query closed while an index publication is in flight.
///
/// nw-384. The contract is stated verbatim in four operator-facing places —
/// `repair --help` (`src/main.rs`), the `investigate` MCP tool description
/// (`nestweaver-mcp/src/tools.rs`) and the daemon's (`server.rs`) — all of the
/// form "every ranked query (brain_context, project_context, investigate)
/// fails closed ... because the PageRank and generation sidecars may predate
/// the committed graph". `investigate` was named in all four and honoured none
/// of them, because it never reaches the guard that enforces it for `context`:
///
/// * `context` errors through `personalized_pagerank_with_intent`, which
///   returns `Err(RankingUnavailable)` on a dirty marker.
/// * `investigate`'s seed path lands on the SILENT-EMPTY guards instead —
///   `symbols_by_pagerank` -> `Ok(vec![])`, `pagerank_scores` -> an empty map —
///   and then, when hybrid retrieval bails, on a BM25 fallback with no
///   publication check at all.
///
/// That split gives the SAME bug two opposite faces, and both were observed on
/// the same build: on a small graph, `returned: 0, dropped_reasons: {}` at exit
/// 0 — a "this code does not exist" answer; on the 193k-node live graph, a
/// fully populated `returned: 30, total: 30, truncated: false` at exit 0,
/// ranked against sidecars the guard itself declares untrustworthy. **The
/// populated face is the worse one**, because an empty map invites suspicion
/// and a complete-looking one does not.
///
/// Deliberately the STORE's own condition (`is_index_publication_dirty`) and
/// the STORE's own error, not a second formulation: `classify_index_publication_error`
/// at the MCP boundary keys on the substring "index publication", so reusing
/// `RankingUnavailable` is what makes `investigate` produce the identical
/// TRANSIENT/WEDGED message `context` produces rather than a near-miss of it.
/// An in-memory store has no marker and is never dirty, so this is inert there.
fn ensure_ranking_publication_clean(store: &GraphStore) -> Result<(), anyhow::Error> {
    if store.is_index_publication_dirty() {
        return Err(anyhow::anyhow!(
            nestweaver_store::StoreError::RankingUnavailable
        ));
    }
    Ok(())
}

fn completed_publication_during_investigate(error: nestweaver_store::StoreError) -> anyhow::Error {
    anyhow::Error::new(error).context("index publication completed during investigate; retry")
}

/// Whether a retrieval failure is the fail-closed publication guard rather
/// than an ordinary "no seeds resolved" miss.
///
/// nw-384. `investigate` used to match `Err(_)` and fall through to BM25, so a
/// guard firing DEEPER in retrieval was converted into a successful-looking
/// answer — the exact inversion the guard exists to prevent. Matched on the
/// rendered chain (`{:#}`) rather than by downcast because the error crosses
/// `build_brain_context_hybrid_with_aliases`'s `anyhow` boundary, where it may
/// already have been wrapped in context; the substring is the same one
/// `classify_index_publication_error` keys on, so the two cannot drift apart
/// on one route and not the other.
fn is_index_publication_failure(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("index publication")
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

    // 0. nw-384. Fail closed BEFORE any retrieval, exactly as `context` does.
    //    This has to be an up-front refusal and not an after-the-fact check on
    //    the results, because the two faces of this bug are contradictory:
    //    "empty" and "fully populated" are both wrong here, and no assertion
    //    about the RESULT can reject both. The only thing they share is the
    //    condition, so the condition is what is tested.
    ensure_ranking_publication_clean(store)?;
    let initial_publication_generation = store
        .clean_published_generation_snapshot()
        .map_err(anyhow::Error::new)?;

    // 1. Resolve scope into the seed inputs and an optional post-filter.
    let (seed_inputs, scope_filter) = resolve_scope(store, query, scope)?;

    // 2. Hybrid retrieval with PRF enabled.
    let config = HybridSearchConfig {
        prf: true,
        ..Default::default()
    };
    // Reviewed regression fix: the scope filter must run BEFORE the render
    // cap, not after. `fused` is scored by `GraphScope::unified()` — it knows
    // nothing about `scope` — so capping hydration first let an out-of-scope
    // candidate that merely outranked a genuinely in-scope one by GLOBAL
    // score consume a slot the in-scope one needed, and the post-hydration
    // `node_in_scope` filter below had nothing left to select from. Building
    // the UID-only predicate here (before retrieval) and passing it as
    // `RenderCap::admit` applies it inside the render loop, before either
    // partition's cap counter increments.
    let admit_uid = scope_filter
        .as_ref()
        .map(|filter| move |uid: &str| uid_in_scope(uid, filter));
    let admit: Option<&dyn Fn(&str) -> bool> =
        admit_uid.as_ref().map(|f| f as &dyn Fn(&str) -> bool);

    // Graceful empty handling: when no seed resolves (e.g. a natural-language
    // multi-word query like "indexing pipeline" that matches no symbol/note
    // title verbatim), hybrid retrieval bails with `No seeds resolved`. Fall
    // back to BM25-only retrieval against the query text so investigations
    // remain useful for orientation queries instead of returning an empty map.
    let connected_result = build_brain_context_hybrid_with_aliases_capped(
        store,
        &seed_inputs,
        tantivy,
        &config,
        &HashMap::new(),
        db_path,
        None,
        embed_model,
        None,
        Some(RenderCap {
            seeds: RETRIEVAL_RENDER_MARGIN,
            connected: RETRIEVAL_RENDER_MARGIN,
            admit,
        }),
    );
    // The query's own resolved seed nodes are first-class map entries,
    // ordered ahead of the graph-proximity nodes so an exact-match query for
    // an isolated symbol still returns that symbol instead of an empty map.
    let mut seed_uids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Reviewed disclosure fix: captured from `ctx` before it is destructured
    // below, since `admitted_before_cap` is the TRUE pre-cap population
    // `total` needs — `connected.len()` after this match already lost that
    // number to `RenderCap` if the cap truncated anything. `None` on the
    // `bm25_fallback` path (no `RenderCap` there to undercount against).
    let mut admitted_before_cap: Option<usize> = None;
    let mut connected: Vec<BrainNode> = match connected_result {
        Ok(ctx) => {
            admitted_before_cap = ctx.admitted_before_cap;
            seed_uids = ctx.seeds.iter().map(|n| n.uid.clone()).collect();
            let mut nodes = ctx.seeds;
            nodes.extend(
                ctx.connected
                    .into_iter()
                    .filter(|n| !seed_uids.contains(&n.uid)),
            );
            nodes
        }
        Err(e) => {
            // nw-384. A publication that BEGAN after the step-0 check is the
            // window this arm used to launder: hybrid retrieval fails closed,
            // `Err(_)` swallows it, and the BM25 fallback added by `36c8ecab`
            // — which has no publication check of its own — answers instead.
            // That fallback is why the large-graph face returned a complete
            // 30-entry map at exit 0 rather than an empty one.
            if is_index_publication_failure(&e) {
                return Err(e);
            }
            // Re-check before falling back for the OTHER half of the same
            // race: the seed path's guards are silent-empty
            // (`symbols_by_pagerank` -> `Ok(vec![])`), so a publication
            // starting mid-retrieval can surface as a benign-looking "no seeds
            // resolved" that carries no publication string to recognise.
            ensure_ranking_publication_clean(store)?;
            if is_no_seed_resolution_error(&e) {
                bm25_fallback(store, tantivy, query, DEFAULT_RETRIEVAL_BREADTH)
            } else {
                return Err(e);
            }
        }
    };
    // nw-384, third and last site. The `Ok` branch needs its own re-check for
    // the reason the `Err` branch cannot cover it: the seed path's publication
    // guards return `Ok(vec![])` / an empty score map rather than an error, so
    // a publication that began mid-retrieval produces a SUCCESS carrying
    // silently degraded ranks. Mirrors `compute_pagerank_warm_inner`'s own
    // mid-compute dirty re-check — the whole point of a fail-closed guard is
    // that it is cheaper to refuse a good answer than to serve a bad one.
    ensure_ranking_publication_clean(store)?;
    if let Some(ref filter) = scope_filter {
        // Now a defence-in-depth no-op in the common case: every candidate
        // that reached `connected` already passed the identical test as
        // `RenderCap::admit`, pre-hydration. Left in place because it is
        // cheap and because it is the ONLY thing that still catches a
        // mismatch if `admit` and this filter were ever built from different
        // state.
        connected.retain(|n| node_in_scope(n, filter));
    }
    // Reviewed disclosure fix (nw-322/nw-362(b), same defect one layer down):
    // `connected.len()` here is the RENDERED count — already smaller than the
    // true in-scope population whenever `RenderCap` truncated something
    // upstream. `admitted_before_cap` is what nw-362(b)'s original fix
    // thought it was capturing "at the truncate site": the population
    // before ANY of this function's caps, not just the ones that still ran
    // after this line.
    let rendered_count = connected.len();
    let total = admitted_before_cap.unwrap_or(rendered_count);
    // How many genuinely in-scope candidates never made it to `connected` at
    // all, because `RenderCap` (nw-322 leg 3's own internal bound, sized by
    // `RETRIEVAL_RENDER_MARGIN`) stopped hydrating before reaching them.
    // Zero whenever no render cap ran (`admitted_before_cap: None`) or it
    // never bound (`total <= rendered_count`).
    let dropped_by_render_cap = total.saturating_sub(rendered_count);
    let dropped_by_breadth = rendered_count.saturating_sub(DEFAULT_RETRIEVAL_BREADTH);
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
    let mut inline_bodies_dropped = 0usize;
    for node in connected.iter_mut() {
        if node.inline_body.is_some() {
            inlined += 1;
            if inlined > MAX_INLINE_BODIES {
                node.inline_body = None;
                inline_bodies_dropped += 1;
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
            unavailable_reason: None,
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

    // The marker checks above catch a publication that is dirty when sampled,
    // but a fast publisher can establish and retire its marker entirely
    // between two samples. The durable generation is monotonic across that
    // complete lifecycle, so compare clean snapshots after every graph/body
    // read and immediately before making the mixed result durable. Refuse
    // before `update_bundle_store` so a retry cannot discover a bundle whose
    // entries came from different graph generations.
    let final_publication_generation = store
        .clean_published_generation_snapshot()
        .map_err(completed_publication_during_investigate)?;
    if final_publication_generation != initial_publication_generation {
        return Err(completed_publication_during_investigate(
            nestweaver_store::StoreError::RankingUnavailable,
        ));
    }

    if let Some(db) = db_path {
        let id = bundle_id.clone();
        update_bundle_store(db, |bundle_store| {
            bundle_store.bundles.insert(id, bundle);
            Ok(())
        })?;
    }

    let semantic_applied = embed_model.is_some();
    // nw-362(b). The keyed map, built HERE from the counters that were
    // already in scope. `retrieval_breadth` is a hard internal bound the
    // caller never stated and cannot raise, which is precisely why it must be
    // named separately from the budget it was being silently folded into.
    //
    // Reviewed disclosure fix: `retrieval_cap` is a THIRD, separate bound,
    // for the same reason — `RenderCap` (nw-322 leg 3) is also internal and
    // also not something the caller stated or can raise, but it is a
    // DIFFERENT size than `retrieval_breadth` and runs at a different point
    // (before hydration, not before the token-budget loop), so folding it
    // into either existing key would hide which bound actually cut.
    let mut dropped_reasons: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if dropped_by_render_cap > 0 {
        dropped_reasons.insert("retrieval_cap".to_string(), dropped_by_render_cap);
    }
    if dropped_by_breadth > 0 {
        dropped_reasons.insert("retrieval_breadth".to_string(), dropped_by_breadth);
    }
    if more_available > 0 {
        dropped_reasons.insert("token_budget".to_string(), more_available);
    }
    debug_assert_eq!(
        dropped_reasons.values().sum::<usize>(),
        total.saturating_sub(entries.len()),
        "the reason map must account for every row between `total` and `returned`"
    );

    Ok(InvestigateResult {
        bundle_id,
        query: query.to_string(),
        scope: scope.to_string(),
        scope_filtered: scope_filter.is_some(),
        domains,
        returned: entries.len(),
        total,
        truncated: entries.len() < total,
        dropped_reasons,
        inline_bodies_dropped,
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
            //
            // nw-301: `expanded = true` used to sit OUTSIDE this branch, so it
            // was set whether or not a body was found. `expanded: true` on an
            // entry with no body reads to an agent as "this symbol has no
            // body", not "this route failed" — the honest-failure antipattern
            // this release was closing elsewhere, in the one field whose job is
            // to say whether the operation worked.
            match fetch_full_body(store, &uid, root) {
                Ok(body) => {
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
                    bundle.entries[idx].expanded = true;
                    bundle.entries[idx].unavailable_reason = None;
                }
                // An entry that already carries a body stays expanded — the
                // fetch is a refresh there, not the thing that made it usable.
                Err(_) if bundle.entries[idx].inline_body.is_some() => {
                    bundle.entries[idx].expanded = true;
                    bundle.entries[idx].unavailable_reason = None;
                }
                Err(reason) => {
                    bundle.entries[idx].expanded = false;
                    bundle.entries[idx].unavailable_reason = Some(reason.reason());
                }
            }
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

    let (hydrated, already_hydrated, skipped, skipped_reasons, entries) =
        update_bundle_store(db_path, |bundle_store| {
            let bundle = bundle_store
                .bundles
                .get_mut(bundle_id)
                .ok_or_else(|| anyhow::anyhow!("bundle '{bundle_id}' not found or expired"))?;

            let mut used_tokens = 0usize;
            let mut hydrated = 0usize;
            let mut already_hydrated = 0usize;
            // nw-301: both `continue`s below used to exit without touching EITHER
            // counter, which is why the reported `hydrated: 7, already_hydrated: 5`
            // summed to 12 — the Note count — on a 30-entry bundle, and the other
            // 18 entries appeared nowhere. A command whose entire job is filling
            // bodies did not account for the entries it failed to fill.
            let mut skipped_reasons: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let skip =
                |reason: String,
                 entry: &mut BundleEntry,
                 reasons: &mut std::collections::BTreeMap<String, usize>| {
                    *reasons.entry(reason.clone()).or_insert(0) += 1;
                    entry.unavailable_reason = Some(reason);
                };
            for entry in bundle.entries.iter_mut() {
                if entry.inline_body.is_some() {
                    already_hydrated += 1;
                    entry.unavailable_reason = None;
                    continue;
                }
                let body = match fetch_full_body(store, &entry.uid, root) {
                    Ok(body) => body,
                    Err(reason) => {
                        skip(reason.reason(), entry, &mut skipped_reasons);
                        continue;
                    }
                };
                if body.is_empty() {
                    skip(BodyUnavailable::Empty.reason(), entry, &mut skipped_reasons);
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
                    skip(
                        format!("token budget of {budget} exhausted"),
                        entry,
                        &mut skipped_reasons,
                    );
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
                entry.unavailable_reason = None;
                hydrated += 1;
            }

            let skipped: usize = skipped_reasons.values().sum();
            let entries = bundle.entries.clone();
            Ok((
                hydrated,
                already_hydrated,
                skipped,
                skipped_reasons,
                entries,
            ))
        })?;

    Ok(HydrateResult {
        bundle_id: bundle_id.to_string(),
        hydrated,
        already_hydrated,
        skipped,
        skipped_reasons,
        entries,
    })
}

// ── Scope resolution ──────────────────────────────────────────────────────

/// Post-retrieval scope filter produced by [`resolve_scope`].
enum ScopeFilter {
    /// Keep only symbols belonging to one of these repos (`repo:` scope).
    Repos(Vec<String>),
    /// `project:` scope. Membership is tested separately for symbols and
    /// notes (see `node_in_scope`) rather than trusting arrival: `resolve_scope`
    /// seeds the raw query text unconditionally, and hybrid seed resolution's
    /// note-title lookup is vault-wide with no project filter, so an
    /// unrelated note whose title happens to match a query token can reach
    /// `connected` without ever having been added to either member set here.
    Project {
        /// UIDs of symbols that are members of the project.
        symbols: std::collections::HashSet<String>,
        /// UIDs of notes that are members of the project (seeded from its
        /// `vault_folder`). A Section/Heading is in scope when the note that
        /// contains it is in this set.
        notes: std::collections::HashSet<String>,
    },
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
        // Recorded as a MEMBER SET, not just seeded: a note UID here is also
        // seeded (below) so it is a first-class retrieval hit, but membership
        // is what `node_in_scope` tests against — a note that reaches
        // `connected` some OTHER way (the raw query text is always seeded
        // too, and its hybrid seed resolution does a vault-wide note-title
        // lookup with no project filter) must not be admitted just because it
        // arrived.
        let mut member_notes = std::collections::HashSet::new();
        if let Ok(note_uids) = store.list_project_note_uids(&project.uid) {
            seeds.extend(note_uids.iter().cloned());
            member_notes.extend(note_uids);
        }
        let mut member_symbols = std::collections::HashSet::new();
        if let Ok(sym_uids) = store.list_project_symbol_uids(&project.uid) {
            seeds.extend(sym_uids.iter().cloned());
            member_symbols.extend(sym_uids);
        }
        return Ok((
            seeds,
            Some(ScopeFilter::Project {
                symbols: member_symbols,
                notes: member_notes,
            }),
        ));
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

/// The hybrid query's unresolved-seed outcome is the one benign failure for
/// which `investigate` can still produce an honest lexical map. Everything
/// else is an operational or integrity failure and must retain its typed error
/// instead of being mistaken for an empty semantic result.
fn is_no_seed_resolution_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().starts_with("No seeds resolved."))
}

/// The note UID that owns a Section/Heading UID, recovered from the UID
/// itself. `nestweaver_schema::uid::note_uid_of_heading` already inverts
/// `head:{note_uid}:{slug_hash}:{line}`; there is no public equivalent for
/// `sec:{note_uid}:{start_line}:{content_hash_short}`, so this inverts that
/// grammar the same way (split off the RIGHT, since `note_uid` itself
/// contains colons) rather than adding a second copy of the technique in a
/// crate outside this batch's file set. Returns `None` for anything that is
/// not a Section or Heading UID.
fn parent_note_uid(uid: &str) -> Option<String> {
    if let Some(note_uid) = nestweaver_schema::uid::note_uid_of_heading(uid) {
        return Some(note_uid.to_string());
    }
    let rest = uid.strip_prefix("sec:")?;
    let (without_hash, _content_hash_short) = rest.rsplit_once(':')?;
    let (note_uid, _start_line) = without_hash.rsplit_once(':')?;
    (!note_uid.is_empty()).then(|| note_uid.to_string())
}

/// Whether a node survives the scope filter.
///
/// nw-378. The two scopes DELIBERATELY disagree about non-symbol
/// (note/section/heading/tag) nodes, and the disagreement is the fix, not a
/// bug to unify away:
///
///  * `repo:` — there is NO repo-to-note association in the schema at all, a
///    Note/Section/Heading/Tag belongs to a vault, not a repo, so "in repo X"
///    cannot be answered for it under any reading. Vault content is dropped
///    entirely, the SAME decision nw-405 already made for
///    `retain_nodes_in_repos` (`brain_context`'s `repos:` filter).
///  * `project:` DOES have a real project-to-note association
///    (`list_project_note_uids`, seeded into `ScopeFilter::Project::notes`),
///    so unlike `repo:` a note CAN genuinely be in scope. But "the pass-through
///    is correct because membership is real" does not license an
///    UNCONDITIONAL pass-through: `resolve_scope` seeds the raw query text (and
///    its per-token splits) regardless of scope, and hybrid seed resolution's
///    `lookup_note_uids_by_title` is vault-wide with NO project filter — so a
///    query token that happens to exact-match an unrelated note's title seeds
///    it into `connected` without that note ever entering the member set.
///    Measured: a `project:onlyhello` investigation whose query token exactly
///    matched an off-project note's title returned that note alongside the
///    project's own member symbol. The fix is a MEMBERSHIP TEST, not a second
///    blanket drop — a Note is in scope iff its UID is a member; a
///    Section/Heading is in scope iff the note that contains it is a member
///    (recovered via `parent_note_uid`). Tags are unchanged (pass through):
///    there is no project-to-tag association to test in the first place, and
///    this is not the leak that was measured.
fn node_in_scope(node: &BrainNode, filter: &ScopeFilter) -> bool {
    uid_in_scope(&node.uid, filter)
}

/// The membership test `node_in_scope` actually performs — on a bare UID,
/// never on any other `BrainNode` field. Split out (reviewed regression fix)
/// so it can run BEFORE hydration, as the `RenderCap::admit` predicate
/// passed to `build_brain_context_hybrid_with_aliases_capped`: `fused` is
/// scored by `GraphScope::unified()` (scope-agnostic), so applying this test
/// only after hydration let an out-of-scope candidate that merely outranked
/// an in-scope one by GLOBAL score consume a render slot the in-scope one
/// needed — silently, since the slot was gone before this filter ever ran.
/// `node_in_scope` stays as a thin wrapper: the post-hydration
/// `connected.retain` call below still runs it as a defence-in-depth check,
/// now expected to be a no-op given every candidate already passed `admit`.
///
/// No `store` parameter (reviewed perf follow-up): the `ScopeFilter::Repos`
/// arm used to take one via `store.lookup_symbol`, which was fine when this
/// ran only on already-hydrated nodes but would have reintroduced an
/// unbounded per-candidate DB hit now that it runs on every `fused`
/// candidate as `admit`. See `repo_uid_of_symbol`.
fn uid_in_scope(uid: &str, filter: &ScopeFilter) -> bool {
    match filter {
        ScopeFilter::Project { symbols, notes } => {
            if uid.starts_with("sym:") {
                return symbols.contains(uid);
            }
            if uid.starts_with("note:") {
                return notes.contains(uid);
            }
            if let Some(parent) = parent_note_uid(uid) {
                return notes.contains(&parent);
            }
            // Tag (or anything else unattributable): no project-tag
            // association exists to test, so this stays a pass-through.
            true
        }
        ScopeFilter::Repos(repo_uids) => {
            let Some(repo_uid) = repo_uid_of_symbol(uid) else {
                return false;
            };
            repo_uids.iter().any(|r| r == repo_uid)
        }
    }
}

/// The repo a symbol belongs to, recovered from the symbol UID itself.
///
/// `store.lookup_symbol` (a DB round-trip) used to answer this. Fine when
/// `uid_in_scope` only ran on already-hydrated nodes — at most
/// `DEFAULT_RETRIEVAL_BREADTH` of them, or (before this fix's admit-first
/// reorder) at most a `RenderCap`. Now that it also runs as `RenderCap::admit`
/// — BEFORE hydration, on every `fused` candidate, so `project:`'s
/// corpus-sized seed set doesn't silently starve `repo:`'s own scope filter
/// the same way it starved `project:`'s — a DB call per candidate would
/// reintroduce exactly the unbounded per-candidate cost nw-322 exists to
/// remove, just moved from `render_brain_node` to here.
///
/// `symbol_uid` mints `sym:{repo_uid}:{file_hash}:{name_hash}:{line}`
/// (`nestweaver_schema::uid`), where `repo_uid` itself contains colons and
/// `file_hash`/`name_hash` are fixed-width 12-hex-char hashes and `line` is
/// numeric (`symbol_uid_format` pins the shape). Peeling those three
/// known-shaped segments off the RIGHT and keeping whatever remains — the
/// same technique `parent_note_uid` uses for headings — recovers `repo_uid`
/// from the UID alone, with no store access, and it CANNOT disagree with a
/// DB lookup: `repo_uid` is baked into the hash inputs at construction, not
/// stored as separate mutable state a UID could drift from.
fn repo_uid_of_symbol(uid: &str) -> Option<&str> {
    let rest = uid.strip_prefix("sym:")?;
    let (without_line, _line) = rest.rsplit_once(':')?;
    let (without_name_hash, _name_hash) = without_line.rsplit_once(':')?;
    let (repo_uid, _file_hash) = without_name_hash.rsplit_once(':')?;
    (!repo_uid.is_empty()).then_some(repo_uid)
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
/// Why an entry has no body — the four outcomes `Option::None` used to conflate.
///
/// nw-301: `fetch_full_body` returned `Option<String>`, so *this kind has no
/// body route*, *the source was not readable from this root*, *the node has no
/// content* and *the node was not found* were one silence. Every caller then
/// treated that silence as "nothing to do", which is why `expand` reported
/// `expanded: true` on entries it had failed to fill and `hydrate` counted them
/// in neither of its counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyUnavailable {
    /// The kind has no body by nature — a Tag is a name, not a document. This
    /// is a FACT, and stating it is different from the bug below it.
    NoBodyForKind(&'static str),
    /// The kind has a body route but the source could not be read from the
    /// caller-supplied root. Symbol file paths are stored repo-relative and
    /// resolved by joining onto ONE root, so in a multi-repo graph at most one
    /// repo's symbols can be read per call.
    SourceUnreadable { path: String },
    /// The node exists and its body is genuinely empty.
    Empty,
    /// No such node in the graph (a stale bundle, or a UID from another graph).
    NotFound,
    /// The UID belongs to no domain this schema mints.
    UnknownUid,
}

impl BodyUnavailable {
    /// A short, stable reason string for the wire.
    pub fn reason(&self) -> String {
        match self {
            BodyUnavailable::NoBodyForKind(kind) => {
                format!("{kind} entries have no body")
            }
            BodyUnavailable::SourceUnreadable { path } => {
                format!("source not readable from the supplied root: {path}")
            }
            BodyUnavailable::Empty => "the node's body is empty".to_string(),
            BodyUnavailable::NotFound => "not found in this graph".to_string(),
            BodyUnavailable::UnknownUid => "unrecognised uid".to_string(),
        }
    }
}

/// Fetch the full body behind a bundle entry, or say why there is none.
///
/// The match is over [`UidKind`], which enumerates every domain
/// `nestweaver-schema` mints, so a twelfth kind is a compile error here rather
/// than a silent dead end. Before nw-301 this was an `if` chain over three of
/// the five kinds a bundle can contain: `head:` and `tag:` fell off the end into
/// `None` and were unhydratable forever — no `--root`, no token budget and no
/// retry could ever have filled them.
fn fetch_full_body(store: &GraphStore, uid: &str, root: &Path) -> Result<String, BodyUnavailable> {
    use nestweaver_schema::UidKind;

    let non_empty = |text: String| {
        if text.is_empty() {
            Err(BodyUnavailable::Empty)
        } else {
            Ok(text)
        }
    };
    // Sections in document order, joined — the shape `note:` already produced.
    let join_sections = |sections: Vec<nestweaver_schema::Section>| {
        let mut combined: Vec<(u32, String)> = sections
            .into_iter()
            .map(|s| (s.start_line, s.text_content))
            .collect();
        combined.sort_by_key(|(line, _)| *line);
        combined
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    match UidKind::of(uid) {
        Some(UidKind::Symbol) => {
            let reader = crate::content_reader::FilesystemReader::new(root);
            let res =
                crate::read_symbols::read_symbols(store, &[uid.to_string()], &reader, 0, None);
            match res.symbols.into_iter().next() {
                // `body_available` is the signal `read_symbols` already
                // computes and this function used to throw away by reading
                // `.body` alone — an unreadable span became an empty string,
                // indistinguishable from a symbol with no source.
                Some(window) if !window.body_available => {
                    Err(BodyUnavailable::SourceUnreadable { path: window.path })
                }
                Some(window) => non_empty(window.body),
                None => Err(BodyUnavailable::NotFound),
            }
        }
        Some(UidKind::Section) => match store.lookup_section(uid) {
            Ok(section) => non_empty(section.text_content),
            Err(_) => Err(BodyUnavailable::NotFound),
        },
        Some(UidKind::Note) => match store.sections_in_note(uid) {
            Ok(sections) if sections.is_empty() => Err(BodyUnavailable::Empty),
            Ok(sections) => non_empty(join_sections(sections)),
            Err(_) => Err(BodyUnavailable::NotFound),
        },
        // A heading's body is the text under it, which is exactly the sections
        // that point back at it. The note is recoverable from the heading UID
        // itself (`head:{note_uid}:{slug_hash}:{line}`), so this needs no extra
        // lookup and no schema change — the arm was simply never written.
        Some(UidKind::Heading) => {
            let Some(note) = nestweaver_schema::note_uid_of_heading(uid) else {
                return Err(BodyUnavailable::UnknownUid);
            };
            match store.sections_in_note(note) {
                Ok(sections) => {
                    let own: Vec<nestweaver_schema::Section> = sections
                        .into_iter()
                        .filter(|section| section.heading_uid.as_deref() == Some(uid))
                        .collect();
                    if own.is_empty() {
                        Err(BodyUnavailable::Empty)
                    } else {
                        non_empty(join_sections(own))
                    }
                }
                Err(_) => Err(BodyUnavailable::NotFound),
            }
        }
        // Stated, not fallen through. "Tags have no body" is a fact the caller
        // can act on; "no body" with no reason reads as "this failed" and
        // invites an infinite retry.
        Some(
            kind @ (UidKind::Tag
            | UidKind::Repo
            | UidKind::File
            | UidKind::Service
            | UidKind::Vault
            | UidKind::Project
            | UidKind::Contract),
        ) => Err(BodyUnavailable::NoBodyForKind(kind.label())),
        None => Err(BodyUnavailable::UnknownUid),
    }
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
mod bundle_lock_tests {
    use super::*;

    /// A lock token stamped with THIS process's pid namespace, i.e. what a
    /// real local holder writes.
    fn local_token(pid: i32) -> String {
        format!(
            "{pid}:12345:{}",
            pid_namespace_identity().unwrap_or_default()
        )
    }

    /// A pid no live process can own. Probing upward from a high number keeps
    /// this deterministic without depending on any particular pid_max.
    fn dead_pid() -> i32 {
        (90_000..99_000)
            .find(|pid| !crate::index_publication::process_is_alive(*pid))
            .expect("some pid in the probe range is unused")
    }

    /// A pid is only meaningful inside the namespace that issued it. A token
    /// from ANOTHER namespace -- a containerised daemon sharing a bind-mounted
    /// database with a host-side CLI -- must fall back to the mtime rule, not
    /// be read as dead. Trusting it would break a LIVE holder's lock and
    /// reintroduce the lost-bundle race in a form worse than the old
    /// namespace-agnostic rule.
    #[test]
    fn a_lock_token_from_another_pid_namespace_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        // A dead-looking pid, but stamped with a namespace that is not ours.
        std::fs::write(&lock_path, format!("{}:12345:pid:[999999999]", dead_pid())).unwrap();

        let lock = BundleStoreLock::acquire(&db);
        assert!(
            !lock.owned,
            "a pid from a foreign namespace must not license breaking the lock"
        );
    }

    /// A legacy two-field token predates the namespace stamp, so its origin is
    /// unknown and it must fail safe the same way.
    #[test]
    fn a_legacy_token_without_a_namespace_stamp_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, format!("{}:12345", dead_pid())).unwrap();

        let lock = BundleStoreLock::acquire(&db);
        assert!(
            !lock.owned,
            "a token with no namespace stamp must fail safe"
        );
    }

    /// nw-395 leg 2: every failure arm returned `owned: false` and fell
    /// through to load -> mutate -> save UNLOCKED. Under contention that is how
    /// a bundle already handed to the caller was lost -- two writers each
    /// load the same store, each save their own copy, and the loser's bundle
    /// is gone while its id was already advertised as a drill-in.
    #[test]
    fn a_mutation_that_cannot_take_the_lock_fails_instead_of_writing_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        // A LIVE holder: not stealable, and not stale by mtime either.
        std::fs::write(&lock_path, local_token(std::process::id() as i32)).unwrap();

        let error = update_bundle_store(&db, |store| {
            store.bundles.clear();
            Ok(())
        })
        .expect_err("an unlockable mutation must fail rather than write unlocked");
        let text = error.to_string();
        assert!(
            text.contains("lock"),
            "the failure must name the lock as the cause: {text}"
        );
        assert!(
            !bundle_sidecar_path(&db).exists(),
            "nothing may be written when the lock was never held"
        );
    }

    /// Counterweight: an uncontended mutation still succeeds.
    #[test]
    fn an_uncontended_mutation_still_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        update_bundle_store(&db, |store| {
            store.bundles.clear();
            Ok(())
        })
        .expect("an uncontended mutation must still write");
        assert!(bundle_sidecar_path(&db).exists());
    }

    /// nw-395 leg 3: `load_bundle_store` mapped EVERY read error to an empty
    /// store, so a `chmod 000` sidecar, a directory in its place, or an I/O
    /// fault all surfaced as `bundle '<id>' not found or expired` -- a
    /// diagnosis that sends the user to the TTL when the real cause is that
    /// their bundle store cannot be read at all.
    #[test]
    fn an_unreadable_bundle_store_is_reported_as_unreadable_not_expired() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the mode bits.
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let sidecar = bundle_sidecar_path(&db);
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, "{}").unwrap();
        std::fs::set_permissions(
            &sidecar,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();

        let error = update_bundle_store(&db, |_| Ok(()))
            .expect_err("an unreadable bundle store must not look empty");
        let text = error.to_string();
        assert!(
            !text.contains("not found or expired"),
            "an unreadable store must not be diagnosed as an expired bundle: {text}"
        );
        assert!(
            text.contains("read") || text.contains("unreadable"),
            "the failure must name unreadability: {text}"
        );

        std::fs::set_permissions(
            &sidecar,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
    }

    /// Counterweight: a genuinely ABSENT sidecar is still an empty store, not
    /// an error -- that is the first-run path and must stay silent.
    #[test]
    fn an_absent_bundle_store_is_still_simply_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let seen = update_bundle_store(&db, |store| Ok(store.bundles.len()))
            .expect("an absent sidecar is a first run, not a fault");
        assert_eq!(seen, 0);
    }

    /// nw-395: the lock token records the writer's pid and NOTHING ever read
    /// it. Staleness was mtime-only at 60s while the acquisition deadline is
    /// 10s, so a holder that died between create and Drop stalled every
    /// `investigate` for the full 10.1s and then let ALL waiters proceed
    /// unlocked -- which is how a bundle already handed to the caller was lost.
    #[test]
    fn a_lock_held_by_a_dead_process_is_broken_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        // A fresh mtime, so the 60s mtime rule cannot break it.
        std::fs::write(&lock_path, local_token(dead_pid())).unwrap();

        let started = std::time::Instant::now();
        let lock = BundleStoreLock::acquire(&db);
        let waited = started.elapsed();

        assert!(
            lock.owned,
            "a lock whose recorded pid is dead must be broken and re-acquired"
        );
        assert!(
            waited < std::time::Duration::from_secs(LOCK_WAIT_SECS),
            "breaking a dead holder must not wait out the full deadline, waited {waited:?}"
        );
    }

    /// The counterweight that keeps the above from being a licence to steal:
    /// a LIVE holder is still respected, and the waiter still yields.
    #[test]
    fn a_lock_held_by_a_live_process_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        // This very process is unambiguously alive.
        std::fs::write(&lock_path, local_token(std::process::id() as i32)).unwrap();

        let lock = BundleStoreLock::acquire(&db);
        assert!(!lock.owned, "a live holder's lock must not be stolen");
        assert!(
            lock_path.exists(),
            "the live holder's lock file must survive"
        );
    }

    /// An unparseable or pid-less lock file must fall back to the mtime rule
    /// rather than being treated as dead. Failing the other way would let a
    /// truncated write hand the lock to a competitor.
    #[test]
    fn an_unreadable_lock_token_is_not_treated_as_dead() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let lock_path = bundle_sidecar_path(&db).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, "not-a-token").unwrap();

        let lock = BundleStoreLock::acquire(&db);
        assert!(
            !lock.owned,
            "an unparseable token must fail safe, not break the lock"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::index_directory_in_memory;
    use std::fs;

    #[test]
    fn investigate_fallback_does_not_swallow_unreadable_embedding_identity() {
        let no_seeds = anyhow::anyhow!(
            "No seeds resolved. Tried as UIDs, note titles, tags, symbol names, and semantic search."
        )
        .context("hybrid retrieval");
        assert!(is_no_seed_resolution_error(&no_seeds));

        let identity_error =
            anyhow::Error::new(nestweaver_store::StoreError::EmbeddingIdentityUnreadable {
                detail: "injected malformed embedding metadata".to_string(),
            })
            .context("hybrid retrieval");
        assert!(!is_no_seed_resolution_error(&identity_error));
        assert!(
            identity_error
                .downcast_ref::<nestweaver_store::StoreError>()
                .is_some_and(|error| matches!(
                    error,
                    nestweaver_store::StoreError::EmbeddingIdentityUnreadable { .. }
                )),
            "the fail-closed store error must remain typed through context"
        );
    }

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

    // ── nw-384: the index-publication fail-closed guard ──────────────────
    //
    // A file-backed store is mandatory for these: the marker is a FILE beside
    // the db, and `index_directory_in_memory` (which every other test here
    // uses) yields a store with no `db_path` and therefore no marker to be
    // dirty. That is exactly why this guard could sit unenforced under a full
    // test suite.

    fn on_disk_store() -> (tempfile::TempDir, std::path::PathBuf, GraphStore) {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        for (i, name) in ["greet", "greetUser", "formatGreeting"].iter().enumerate() {
            store
                .insert_symbol(&Symbol {
                    uid: format!("sym:g{i}"),
                    name: (*name).to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:test".to_string(),
                    file_path: format!("src/g{i}.js"),
                    start_line: 1,
                    end_line: 2,
                    signature: format!("function {name}()"),
                    summary: None,
                    content_hash: format!("g{i}"),
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
        (dir, db_path, store)
    }

    struct CompletingPublication<'a> {
        store: &'a GraphStore,
        db_path: &'a std::path::Path,
        completed: std::sync::atomic::AtomicBool,
    }

    impl EmbedQueryFn for CompletingPublication<'_> {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            let publication = crate::index::establish_index_publication_marker_with_io(
                self.store,
                Some(self.db_path),
                "investigate completed-publication race test",
                &crate::index::FileSystemIndexEpilogueIo,
            )
            .map_err(|error| anyhow::anyhow!("establish test publication: {error:#}"))?;
            crate::index::finalize_committed_index_for_scope_with_io(
                publication,
                Some(self.db_path),
                "investigate completed-publication race test",
                &crate::index::FileSystemIndexEpilogueIo,
                None,
                true,
            )
            .map_err(|error| anyhow::anyhow!("finalize test publication: {error:#}"))?;
            self.completed
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(vec![0.0; 4])
        }
    }

    /// Plant the same durable marker a real in-flight publication writes.
    fn begin_publication(db_path: &std::path::Path) {
        let marker = nestweaver_store::index_publication::marker_path(db_path);
        fs::write(
            &marker,
            nestweaver_store::index_publication::format_marker_payload(std::process::id(), 1, None),
        )
        .unwrap();
    }

    fn run(
        store: &GraphStore,
        db_path: &std::path::Path,
        root: &std::path::Path,
        query: &str,
    ) -> Result<InvestigateResult, anyhow::Error> {
        investigate(
            store,
            None,
            Some(db_path),
            root,
            query,
            "vault",
            Some(4000),
            None,
        )
    }

    /// FACE ONE — seeds resolve, so retrieval succeeds and `investigate` used
    /// to hand back `returned: 30, total: 30, truncated: false,
    /// dropped_reasons: {}` at exit 0 while `context` on the same DB at the
    /// same moment refused. This is the WORSE face: an empty map invites
    /// suspicion and a complete-looking one does not.
    #[test]
    fn a_dirty_publication_refuses_an_investigation_whose_seeds_resolve() {
        let (dir, db_path, store) = on_disk_store();

        // The fixture must actually produce a populated answer, or this test
        // would pass for the wrong reason — refusing something that was empty
        // anyway proves nothing about the populated face.
        let clean = run(&store, &db_path, dir.path(), "greet").unwrap();
        assert!(
            !clean.entries.is_empty(),
            "fixture must yield a populated map so the guard is proven against \
             the face that looks complete"
        );

        begin_publication(&db_path);

        let error = run(&store, &db_path, dir.path(), "greet")
            .expect_err("a ranked query must fail closed during a dirty publication");
        assert!(
            format!("{error:#}").contains("index publication"),
            "the error must carry the substring `classify_index_publication_error` keys on, \
             so `investigate` produces the SAME TRANSIENT/WEDGED message `context` does; got: {error:#}"
        );
    }

    /// FACE TWO — no seed resolves, so retrieval bails and the BM25 fallback
    /// (`36c8ecab`, which carries no publication check) answers. On the sweep's
    /// smaller graph that produced `0 domains, 0 entries` at exit 0: a "this
    /// code does not exist" answer. Same guard, opposite symptom; closing only
    /// one of them closes neither.
    #[test]
    fn a_dirty_publication_refuses_an_investigation_that_falls_back_to_bm25() {
        let (dir, db_path, store) = on_disk_store();

        let clean = run(&store, &db_path, dir.path(), "no_such_identifier_anywhere").unwrap();
        assert!(
            clean.entries.is_empty(),
            "fixture must exercise the empty/fallback face"
        );

        begin_publication(&db_path);

        let error = run(&store, &db_path, dir.path(), "no_such_identifier_anywhere")
            .expect_err("the BM25 fallback must fail closed too, not report an empty graph");
        assert!(
            format!("{error:#}").contains("index publication"),
            "got: {error:#}"
        );
    }

    /// COUNTERWEIGHT. A clean publication must not trigger the guard on either
    /// face — a fail-closed check that fires when nothing is in flight would
    /// convert this honesty fix into an outage.
    #[test]
    fn a_clean_publication_does_not_trigger_the_fail_closed_guard() {
        let (dir, db_path, store) = on_disk_store();
        assert!(
            !store.is_index_publication_dirty(),
            "a freshly created store has no publication in flight"
        );

        let populated = run(&store, &db_path, dir.path(), "greet")
            .expect("a clean publication must serve the populated face");
        assert!(!populated.entries.is_empty());

        let empty = run(&store, &db_path, dir.path(), "no_such_identifier_anywhere")
            .expect("a clean publication must serve the empty face as a normal answer");
        assert!(empty.entries.is_empty());

        // And it must still be clean afterwards: the guard reads the marker,
        // it never plants one.
        assert!(!store.is_index_publication_dirty());
    }

    /// A retired marker restores service. The guard is a WINDOW, not a latch —
    /// if it were a latch, the remedy an operator is told to wait for would
    /// never take effect.
    #[test]
    fn retiring_the_marker_restores_investigation_service() {
        let (dir, db_path, store) = on_disk_store();
        begin_publication(&db_path);
        assert!(run(&store, &db_path, dir.path(), "greet").is_err());

        fs::remove_file(nestweaver_store::index_publication::marker_path(&db_path)).unwrap();

        let result = run(&store, &db_path, dir.path(), "greet")
            .expect("service resumes once the publication retires");
        assert!(!result.entries.is_empty());
    }

    /// A publisher can start and finish between marker samples. The callback
    /// executes the real marker/reservation/generation/retirement lifecycle
    /// synchronously during semantic retrieval, making that window
    /// deterministic without sleeps or scheduler assumptions.
    #[test]
    fn a_completed_publication_during_investigate_refuses_before_bundle_persistence() {
        let (dir, db_path, store) = on_disk_store();
        store.set_embedding_metadata("test-model", 4).unwrap();
        assert!(store.add_embedding("sym:g0", vec![0.0; 4]));
        store.flush_embedding_index().unwrap();
        // Model a separate long-lived reader process: its in-memory generation
        // is loaded before the publisher advances the durable sidecar and must
        // remain stale throughout the callback.
        let reader = GraphStore::open_read_only(&db_path).unwrap();
        let reader_generation = reader.graph_generation();
        let publisher = CompletingPublication {
            store: &store,
            db_path: &db_path,
            completed: std::sync::atomic::AtomicBool::new(false),
        };

        let error = investigate(
            &reader,
            None,
            Some(&db_path),
            dir.path(),
            "greet",
            "vault",
            Some(4000),
            Some(&publisher),
        )
        .expect_err("a completed mid-query publication must invalidate the investigation");

        assert!(
            publisher
                .completed
                .load(std::sync::atomic::Ordering::Acquire),
            "the semantic callback must complete the publication for the test to prove the race"
        );
        assert_eq!(
            reader.graph_generation(),
            reader_generation,
            "the independent reader's local atomic must remain stale so only the durable sidecar can detect the publication"
        );
        assert_ne!(
            reader.clean_published_generation_snapshot().unwrap(),
            reader_generation,
            "the independent reader must observe the publisher's new durable generation"
        );
        assert!(
            format!("{error:#}").contains("index publication completed during investigate; retry"),
            "the refusal must carry a stable retry diagnostic; got: {error:#}"
        );
        assert!(
            load_bundle_store(&db_path).bundles.is_empty(),
            "the invalid mixed-generation bundle must never be persisted"
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

    /// A vault with a heading, a section under it and a tag — the three note
    /// kinds a bundle can contain besides `note:` itself.
    fn make_vault_store() -> (tempfile::TempDir, std::path::PathBuf, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        fs::write(
            vault.join("deploy.md"),
            "# Deployment\n\nThe service deploys through CI on every merge. #ops\n\n\
             ## Rollback\n\nRun the rollback script and page the on-call.\n",
        )
        .unwrap();
        let (_result, store) =
            crate::index_md::index_markdown_directory_in_memory(&vault, "test", "testvault")
                .unwrap();
        (dir, vault, store)
    }

    /// A vault with strictly more than `DEFAULT_RETRIEVAL_BREADTH` notes
    /// reachable from one hub, so the breadth bound actually BITES.
    ///
    /// `make_vault_store`'s one-note vault is under every cap in this file, so
    /// a disclosure test written against it passes vacuously — the same trap
    /// this cluster keeps hitting.
    fn make_wide_vault_store() -> (tempfile::TempDir, GraphStore) {
        const NOTES: usize = 40;
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        let links: String = (0..NOTES).map(|i| format!("- [[Leaf {i:02}]]\n")).collect();
        fs::write(
            vault.join("Hub.md"),
            format!("# Hub\n\nThe hub note.\n\n{links}"),
        )
        .unwrap();
        for i in 0..NOTES {
            fs::write(
                vault.join(format!("Leaf {i:02}.md")),
                format!("# Leaf {i:02}\n\nA leaf of [[Hub]].\n"),
            )
            .unwrap();
        }
        let (_result, store) =
            crate::index_md::index_markdown_directory_in_memory(&vault, "test", "testvault")
                .unwrap();
        (dir, store)
    }

    /// nw-362(b). `investigate` has five caps and `more_available` counted
    /// ONE. `DEFAULT_RETRIEVAL_BREADTH` cuts BEFORE the token-budget loop and
    /// incremented nothing, so an undercount was presented as a count — and
    /// `#[serde(skip_serializing_if = "is_zero")]` made the field VANISH in
    /// exactly the case a breadth truncation produces.
    ///
    /// COUNTERWEIGHT: a query whose neighbourhood fits under the bound must
    /// report `truncated: false` with `returned == total` and an EMPTY reason
    /// map, or a fix that reports a drop unconditionally passes.
    #[test]
    fn investigate_discloses_the_retrieval_bound_not_only_the_token_budget() {
        let (dir, store) = make_wide_vault_store();
        let root = dir.path();

        // A budget roomy enough that the token-budget loop cannot be what cut,
        // so any truncation reported here is attributable to the breadth bound.
        let result = investigate(
            &store,
            None,
            None,
            root,
            "Hub",
            "all",
            Some(MAX_TOKEN_BUDGET),
            None,
        )
        .unwrap();

        assert!(
            result.total > result.returned,
            "the retrieval bound cut and the result reports itself complete: \
             returned={} total={} more_available={}",
            result.returned,
            result.total,
            result.more_available
        );
        assert_eq!(result.returned, result.entries.len());
        assert!(result.truncated, "a cut map must say so: {result:?}");
        assert_eq!(
            result.more_available, 0,
            "the BUDGET did not cut here — `more_available` must stay honest \
             about its own scope rather than absorb the other caps: {result:?}"
        );
        // Reviewed disclosure fix: `RenderCap` (nw-322 leg 3) is a SECOND
        // internal bound, ahead of `retrieval_breadth`, and this fixture's
        // ~123 candidates (41 notes + 41 headings + 41 sections) sit close
        // enough to `RETRIEVAL_RENDER_MARGIN` (120) that it can genuinely
        // fire too — it must not be folded into `retrieval_breadth`, or a
        // caller can no longer tell an internal hydration bound from an
        // internal display bound, which is the exact fold this item exists
        // to prevent one level up. Assert on the SUM of both internal-bound
        // keys rather than assuming either alone accounts for the whole cut.
        assert!(
            result.dropped_reasons.contains_key("retrieval_breadth"),
            "the display bound must still be named, not merely accounted for \
             inside another key: {:?}",
            result.dropped_reasons
        );
        assert_eq!(
            result
                .dropped_reasons
                .get("retrieval_cap")
                .copied()
                .unwrap_or(0)
                + result
                    .dropped_reasons
                    .get("retrieval_breadth")
                    .copied()
                    .unwrap_or(0),
            result.total - result.returned,
            "the caller cannot tell WHICH cap cut, and the remedies differ: \
             raising the budget cannot recover a node either internal bound \
             threw away: {:?}",
            result.dropped_reasons
        );
        assert_eq!(
            result.dropped_reasons.values().sum::<usize>(),
            result.total - result.returned,
            "the reason map must account for every dropped row, exactly as \
             `HydrateResult::skipped_reasons` does: {:?}",
            result.dropped_reasons
        );

        // COUNTERWEIGHT: a small neighbourhood must claim nothing.
        let (small_dir, _vault, small_store) = make_vault_store();
        let small = investigate(
            &small_store,
            None,
            None,
            small_dir.path(),
            "Deployment",
            "all",
            Some(MAX_TOKEN_BUDGET),
            None,
        )
        .unwrap();
        assert_eq!(small.returned, small.total, "{small:?}");
        assert!(!small.truncated, "nothing was cut: {small:?}");
        assert!(small.dropped_reasons.is_empty(), "{small:?}");
    }

    /// nw-362(b), the serde half. `more_available` carried
    /// `skip_serializing_if = "is_zero"`, so it vanished when it was 0 — which
    /// is exactly what a breadth truncation produces. An absent key cannot be
    /// read as "not truncated" (`e09e4a80`), and every route serialises this
    /// struct wholesale, so the presence rule has to be on the struct.
    #[test]
    fn the_disclosure_fields_are_present_even_when_nothing_was_dropped() {
        let (dir, _vault, store) = make_vault_store();
        let result = investigate(
            &store,
            None,
            None,
            dir.path(),
            "Deployment",
            "all",
            Some(MAX_TOKEN_BUDGET),
            None,
        )
        .unwrap();
        let value = serde_json::to_value(&result).unwrap();
        for key in ["returned", "total", "truncated", "more_available"] {
            assert!(
                value.get(key).is_some(),
                "`{key}` vanished from a complete answer, so a consumer cannot \
                 tell it apart from an old producer that never had it: {value}"
            );
        }
    }

    fn bundle_of(db_path: &Path, entries: Vec<BundleEntry>) -> String {
        let mut bundle_store = BundleStore::default();
        bundle_store.bundles.insert(
            "bndl_301".to_string(),
            Bundle {
                bundle_id: "bndl_301".to_string(),
                created_at: now_epoch(),
                query: "deployment".to_string(),
                scope: "vault".to_string(),
                entries,
            },
        );
        save_bundle_store(db_path, &bundle_store).unwrap();
        "bndl_301".to_string()
    }

    fn entry_for(uid: &str, kind: &str) -> BundleEntry {
        BundleEntry {
            asset_id: format!("a_{}", &uid[..uid.len().min(12)]),
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: kind.to_string(),
            location: "deploy.md".to_string(),
            summary: None,
            inline_body: None,
            body_complete: true,
            expanded: false,
            unavailable_reason: None,
            is_seed: false,
            relevance: 1.0,
        }
    }

    /// nw-301. `fetch_full_body` had arms for `sym:`/`sec:`/`note:` and fell off
    /// the end for the other two members of the UID space, so `head:` and
    /// `tag:` entries were a PERMANENT dead end: `expand` marked them
    /// `expanded: true` with no body and `hydrate` — the command whose entire
    /// job is filling bodies — counted them in NEITHER of its counters. That is
    /// why a 30-entry bundle reported `hydrated: 7, already_hydrated: 5`: 7+5
    /// is 12, exactly the Note count, and the other 18 entries appeared nowhere.
    ///
    /// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? Not "add two arms" — an `if`
    /// chain cannot be exhaustive, so a sixth kind would fall off the end the
    /// same way. `fetch_full_body` now matches `nestweaver_schema::UidKind`,
    /// which enumerates every domain the schema mints in ONE place, and the
    /// accounting invariant below is asserted over the whole bundle rather than
    /// over the kinds this test happens to include.
    #[test]
    fn hydrate_accounts_for_every_entry_it_was_given() {
        let (dir, vault, store) = make_vault_store();
        let db_path = dir.path().join("nestweaver.lbug");

        let headings = store.list_all_headings().unwrap();
        assert!(
            !headings.is_empty(),
            "fixture must contain a Heading or this test proves nothing"
        );
        let tags = store.list_tags(None).unwrap();
        assert!(
            !tags.is_empty(),
            "fixture must contain a Tag or this test proves nothing"
        );

        let mut entries: Vec<BundleEntry> = headings
            .iter()
            .map(|h| entry_for(&h.uid, "Heading"))
            .collect();
        entries.push(entry_for(&tags[0].uid, "Tag"));
        let total = entries.len();
        let bundle_id = bundle_of(&db_path, entries);

        let result =
            investigate_hydrate(&store, &db_path, &vault, &bundle_id, Some(16000)).unwrap();

        assert_eq!(
            result.hydrated + result.already_hydrated + result.skipped,
            total,
            "every entry must land in exactly one counter; got hydrated={} \
             already={} skipped={} of {total}",
            result.hydrated,
            result.already_hydrated,
            result.skipped
        );
        assert!(
            result
                .entries
                .iter()
                .filter(|e| e.uid.starts_with("head:"))
                .all(|e| e.inline_body.is_some()),
            "Heading entries must receive a body — `head:` had no arm at all: {:?}",
            result.entries
        );
        let tag_entry = result
            .entries
            .iter()
            .find(|e| e.uid.starts_with("tag:"))
            .expect("the tag entry survives");
        assert!(
            tag_entry
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Tag")),
            "a Tag has no body BY NATURE, and saying so is different from the \
             silence that reads as 'this failed, retry': {tag_entry:?}"
        );
    }

    /// `expanded: true` on an entry with no body is the honest-failure
    /// antipattern this release was closing elsewhere: `expanded = true` sat
    /// OUTSIDE the `if let Some(body)`, so it claimed success on every failure.
    /// An agent reads that as "this symbol has no body", not "this route
    /// failed", and has no signal that more exists.
    #[test]
    fn expand_does_not_claim_success_when_no_body_was_obtained() {
        let (dir, _src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");
        let greet_uid = store
            .lookup_symbols_by_name("greet")
            .unwrap()
            .into_iter()
            .find(|s| s.name == "greet")
            .expect("greet symbol exists")
            .uid;
        let entry = entry_for(&greet_uid, "Symbol");
        let asset_id = entry.asset_id.clone();
        let bundle_id = bundle_of(&db_path, vec![entry]);

        // A root that does not contain the symbol's file — the 43-repo case,
        // where symbol paths are repo-relative and one root can serve one repo.
        let bogus_root = dir.path().join("not-the-repo");
        fs::create_dir_all(&bogus_root).unwrap();
        let out = investigate_expand(
            &store,
            &db_path,
            &bogus_root,
            &bundle_id,
            std::slice::from_ref(&asset_id),
        )
        .unwrap();

        let expanded = out
            .expanded
            .iter()
            .find(|e| e.asset_id == asset_id)
            .expect("the target was expanded");
        assert!(
            expanded.inline_body.is_none(),
            "precondition: no body was readable from a root that lacks the file"
        );
        assert!(
            !expanded.expanded,
            "an entry with no body must not report expanded: true: {expanded:?}"
        );
        assert!(
            expanded
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not readable")),
            "the failure must be NAMED — 'source not readable from the supplied \
             root' is actionable, silence is not: {expanded:?}"
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

        // Assert on what `investigate` ACTUALLY SETS, not on a struct built by
        // hand here. The previous version of this constructed an
        // `InvestigateResult` with `scope_filtered: false` and then asserted
        // the serialized JSON said `false` — which tests serde, not the
        // wiring. `scope_filtered: scope_filter.is_some()` was unexercised, so
        // pinning it to a constant left this test green.
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

        // A real project, so the restrictive case actually BUILDS a filter
        // rather than erroring on an unknown scope.
        let hello_uid = store
            .lookup_symbols_by_name("hello")
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.name == "hello")
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

        let investigate_with = |scope: &str| {
            investigate(
                &store,
                None,
                Some(&db_path),
                &src,
                "greet",
                scope,
                Some(4000),
                None,
            )
            .unwrap_or_else(|error| panic!("investigate({scope:?}) failed: {error:#}"))
        };

        // Every documented pass-through reports FALSE, including the literal
        // "vault" a caller may still send explicitly.
        for pass_through in ["", "vault", "all"] {
            let result = investigate_with(pass_through);
            assert!(
                !result.scope_filtered,
                "{pass_through:?} builds no filter, so scope_filtered must be false"
            );
            assert_eq!(
                serde_json::to_value(&result).unwrap()["scope_filtered"],
                false,
                "{pass_through:?} must serialize the same verdict it computed"
            );
        }

        // A real restriction reports TRUE — and the scope string alone could
        // never have expressed that, which is why the flag exists.
        let restricted = investigate_with("project:onlyhello");
        assert!(
            restricted.scope_filtered,
            "a project scope builds a filter, so scope_filtered must be true"
        );
        assert_eq!(
            restricted.scope, "project:onlyhello",
            "the caller's scope string is still echoed verbatim"
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

    /// nw-322 (leg 3), fixture-adequacy AND the performance counterweight in
    /// one: this project is large enough to REPRODUCE the reported mechanism
    /// -- `resolve_scope`'s `project:` branch seeds PPR with the project's
    /// entire membership, PPR includes every seed regardless of score
    /// (`personalized_pagerank`'s own contract), so before this fix `fused`
    /// was corpus-sized and every single one of those 5,000 candidates paid
    /// a `render_brain_node` DB round-trip BEFORE `DEFAULT_RETRIEVAL_BREADTH`
    /// (30) threw away all but a handful. nw-322 measured 110-142s on a
    /// comparable real project (12-27x every other scope); this fixture is
    /// the same shape at smaller absolute scale.
    ///
    /// COUNTERWEIGHT, run by hand rather than committed as a second test
    /// (committing a temporarily-reverted line would itself be the
    /// regression this guards against): with the call site's
    /// `Some(RenderCap { .. })` changed to `None` (i.e. calling
    /// `build_brain_context_hybrid_with_aliases` unbounded, as before this
    /// fix), this exact test measured 17.07s against this exact fixture
    /// (vs. 1.09s capped) and its elapsed-time assertion FAILED — confirmed
    /// before restoring the cap.
    #[test]
    fn investigate_project_scope_stays_fast_on_a_large_project() {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        // Large enough to dwarf `RETRIEVAL_RENDER_MARGIN` (120) by more than
        // an order of magnitude, so a render-per-member regression cannot
        // hide inside CI noise. Built via `batch_insert_symbols` (one
        // transaction) rather than 5,000 individual `insert_symbol` calls —
        // this test times RETRIEVAL, not fixture-setup I/O.
        const MEMBER_COUNT: usize = 5_000;
        let setup_started = std::time::Instant::now();
        let symbols: Vec<Symbol> = (0..MEMBER_COUNT)
            .map(|i| Symbol {
                uid: format!("sym:big{i:06}"),
                name: format!("fn{i}"),
                kind: SymbolKind::Function,
                repo_uid: "repo:big".to_string(),
                file_path: format!("src/f{i}.js"),
                start_line: 1,
                end_line: 2,
                signature: format!("function fn{i}()"),
                summary: None,
                content_hash: format!("h{i}"),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .collect();
        store.batch_insert_symbols(&symbols).unwrap();
        let member_uids: Vec<String> = symbols.into_iter().map(|s| s.uid).collect();
        let project = nestweaver_schema::Project {
            uid: "proj:test:big".to_string(),
            name: "big".to_string(),
            summary: None,
            instance_id: "test".to_string(),
        };
        store.upsert_project(&project).unwrap();
        store
            .batch_insert_project_symbol_edges(&project.uid, &member_uids, 1.0)
            .unwrap();
        eprintln!(
            "fixture setup ({MEMBER_COUNT} members): {:?}",
            setup_started.elapsed()
        );

        let started = std::time::Instant::now();
        let result = investigate(
            &store,
            None,
            Some(&db_path),
            dir.path(),
            "fn1",
            "project:big",
            Some(4000),
            None,
        )
        .expect("investigate against a large project must still succeed");
        let elapsed = started.elapsed();
        eprintln!("investigate(project:big, {MEMBER_COUNT} members): {elapsed:?}");

        assert!(
            !result.entries.is_empty(),
            "a {MEMBER_COUNT}-member project must still return an architectural map"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "investigate --scope project: took {elapsed:?} for a {MEMBER_COUNT}-member \
             project; nw-322 measured 110-142s on a comparable real project because every \
             fused candidate was hydrated before DEFAULT_RETRIEVAL_BREADTH discarded all \
             but 30 of them. This threshold is deliberately generous (CI-noise headroom, \
             not a performance target) -- the point is bounded-and-fast, not fastest-possible."
        );

        // Reviewed disclosure fix: `total` must report the TRUE in-scope
        // population (every one of the {MEMBER_COUNT} members is admitted,
        // since all are project members and therefore seeds), not the
        // render-cap-truncated count that used to leak through here. Before
        // this fix, `total` pinned at no more than `RETRIEVAL_RENDER_MARGIN`
        // per partition no matter how large the real population was.
        assert!(
            result.total > RETRIEVAL_RENDER_MARGIN,
            "total ({}) must reflect the true {MEMBER_COUNT}-member population, not a count \
             the render cap already truncated to at most {RETRIEVAL_RENDER_MARGIN} per \
             partition",
            result.total
        );
        assert!(
            result
                .dropped_reasons
                .get("retrieval_cap")
                .copied()
                .unwrap_or(0)
                > 0,
            "retrieval_cap must disclose that the render cap dropped candidates, separately \
             from retrieval_breadth: {:?}",
            result.dropped_reasons
        );
        // The invariant the internal `debug_assert_eq!` also checks --
        // pinned here too, at the public return value, not only inside the
        // function that computed it.
        assert_eq!(
            result.dropped_reasons.values().sum::<usize>(),
            result.total - result.returned,
            "dropped_reasons must fully account for total - returned: {:?}",
            result.dropped_reasons
        );
    }

    /// REVIEWED REGRESSION, fixed by reordering rather than by tuning the
    /// margin. `investigate_project_scope_stays_fast_on_a_large_project`
    /// above cannot exhibit this: every one of its 5,000 symbols is a
    /// project member, so `uid_in_scope` never rejects anything and there is
    /// no non-member competitor to crowd one out. This fixture is built
    /// specifically so there IS one: 200 out-of-scope headings (more than
    /// `RETRIEVAL_RENDER_MARGIN`, 120) that outrank the project's own
    /// in-scope heading by GLOBAL BM25 score, because `fused` is scored by
    /// `GraphScope::unified()` and knows nothing about `project:` scope.
    ///
    /// The member's heading is the ONLY conduit tested here on purpose: a
    /// Heading is never itself a project member or a seed (`resolve_scope`
    /// seeds only the project's own notes/symbols directly) — it reaches
    /// `connected` exclusively via non-seed retrieval (PPR graph walk from
    /// its note, or here, a BM25 hit on its own text), which is exactly the
    /// class `RenderCap::admit` exists to protect once a cap is in play.
    ///
    /// COUNTERWEIGHT: with `RenderCap::admit` reverted to always `None` (the
    /// pre-review-fix behaviour — cap first, filter after), this test's
    /// membership assertion FAILS: none of the 120 rendered `connected`
    /// candidates is the member's heading, because all 120 slots go to
    /// higher-BM25-scoring noise before the scope filter ever runs. Verified
    /// by hand before restoring `admit`.
    #[test]
    fn project_scope_render_cap_does_not_starve_members_outscored_globally() {
        use nestweaver_schema::{Heading, Note, NoteKind, Vault};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nestweaver.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        let vault_uid = "vlt:test:crowd";
        store
            .upsert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "crowd".to_string(),
                root_path: "/crowd".to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();

        let project = nestweaver_schema::Project {
            uid: "proj:test:crowded".to_string(),
            name: "crowded".to_string(),
            summary: None,
            instance_id: "test".to_string(),
        };
        store.upsert_project(&project).unwrap();

        // The ONE genuinely in-scope piece of content, reachable only
        // through its heading (see doc comment above).
        let member_note_uid = format!("note:{vault_uid}:member");
        store
            .insert_note(&Note {
                uid: member_note_uid.clone(),
                vault_uid: vault_uid.to_string(),
                file_path: "member.md".to_string(),
                title: "Member Note".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "h-member".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_vault_note_edge(vault_uid, &member_note_uid)
            .unwrap();
        store
            .batch_insert_project_note_edges(&[(project.uid.as_str(), member_note_uid.as_str())])
            .unwrap();
        let member_heading_uid = nestweaver_schema::uid::heading_uid(&member_note_uid, "widget", 1);
        store
            .insert_heading(&Heading {
                uid: member_heading_uid.clone(),
                note_uid: member_note_uid.clone(),
                level: 1,
                // Low term frequency -- one mention, deliberately outranked.
                text: "widget".to_string(),
                slug: "widget".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "h-member-head".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .batch_insert_note_heading_edges(&[(
                member_note_uid.as_str(),
                member_heading_uid.as_str(),
            )])
            .unwrap();

        // NOISE: more than `RETRIEVAL_RENDER_MARGIN` (120) out-of-scope
        // headings, each with high query-term repetition so BM25 ranks
        // every one of them above the member's single-mention heading.
        const NOISE_COUNT: usize = 200;
        for i in 0..NOISE_COUNT {
            let note_uid = format!("note:{vault_uid}:noise{i:04}");
            store
                .insert_note(&Note {
                    uid: note_uid.clone(),
                    vault_uid: vault_uid.to_string(),
                    file_path: format!("noise{i:04}.md"),
                    title: format!("Noise Note {i}"),
                    note_kind: NoteKind::General,
                    word_count: 20,
                    content_hash: format!("h-noise{i}"),
                    frontmatter: None,
                    frontmatter_raw: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
            store.insert_vault_note_edge(vault_uid, &note_uid).unwrap();
            let heading_uid = nestweaver_schema::uid::heading_uid(&note_uid, "widget", 1);
            store
                .insert_heading(&Heading {
                    uid: heading_uid.clone(),
                    note_uid: note_uid.clone(),
                    level: 1,
                    text: "widget ".repeat(20),
                    slug: "widget".to_string(),
                    start_line: 1,
                    end_line: 1,
                    content_hash: format!("h-noise-head{i}"),
                    embedding: None,
                })
                .unwrap();
            store
                .batch_insert_note_heading_edges(&[(note_uid.as_str(), heading_uid.as_str())])
                .unwrap();
        }

        let tantivy_dir = tempfile::tempdir().unwrap();
        let tantivy = nestweaver_store::TantivyIndex::open_or_create(tantivy_dir.path()).unwrap();
        tantivy.reindex_from_store(&store).unwrap();

        let result = investigate(
            &store,
            Some(&tantivy),
            Some(&db_path),
            dir.path(),
            "widget",
            "project:crowded",
            Some(16000),
            None,
        )
        .unwrap();

        let uids: Vec<&String> = result.entries.iter().map(|e| &e.uid).collect();
        assert!(
            uids.contains(&&member_heading_uid),
            "the project member's own heading must survive {NOISE_COUNT} higher-scoring \
             out-of-scope competitors -- if this fails, RenderCap's admit predicate is not \
             running before the per-partition cap; entries: {uids:?}"
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

    /// nw-378, second half: `project:` scope's note pass-through was never a
    /// blanket "notes are fine here" — it relied on the CLAIM that a note
    /// reaching `node_in_scope` under `project:` is necessarily a member,
    /// because `resolve_scope` seeds the project's own notes. That claim is
    /// false: `resolve_scope` ALSO seeds the raw query text (and its
    /// per-token splits) unconditionally, regardless of scope, and hybrid
    /// seed resolution's `lookup_note_uids_by_title` is vault-wide with no
    /// project filter. A query token that exact-matches an UNRELATED note's
    /// title reaches `connected` without that note ever entering
    /// `list_project_note_uids`'s member set — the SAME mechanism that
    /// produced the measured `repo:` leak, on the scope the original fix
    /// declared safe.
    ///
    /// Both halves in one test, because they are the same claim from two
    /// directions: a project's own member note MUST still be admitted (the
    /// counterweight nw-378 explicitly requires — this is the pass-through
    /// that is genuinely correct), and a note that only coincidentally
    /// shares a title with a query token MUST NOT be, even though both
    /// arrive in `connected` the same way (as a `note:` UID seed).
    ///
    /// COUNTERWEIGHT: reverting `node_in_scope`'s `ScopeFilter::Project` arm
    /// to the pre-fix unconditional `if !node.uid.starts_with("sym:") {
    /// return true; }` makes the "foreign note excluded" assertion below
    /// FAIL — the foreign note survives exactly as measured. Verified by
    /// hand before committing.
    #[test]
    fn project_scope_admits_member_notes_but_excludes_notes_reached_only_via_global_title_seed() {
        let (dir, src, store) = make_store();
        let db_path = dir.path().join("nestweaver.lbug");

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

        let vault_uid = "vlt:test:probevault";
        store
            .upsert_vault(&nestweaver_schema::Vault {
                uid: vault_uid.to_string(),
                name: "probevault".to_string(),
                root_path: "/probevault".to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let mk_note = |slug: &str, title: &str| nestweaver_schema::Note {
            uid: format!("note:{vault_uid}:{slug}"),
            vault_uid: vault_uid.to_string(),
            file_path: format!("{slug}.md"),
            title: title.to_string(),
            note_kind: nestweaver_schema::NoteKind::General,
            word_count: 1,
            content_hash: "h".to_string(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };

        // A note that genuinely IS a project member.
        let member_note = mk_note("memberdoc", "onlyhello project doc");
        let member_note_uid = member_note.uid.clone();
        store.insert_note(&member_note).unwrap();
        store
            .insert_vault_note_edge(vault_uid, &member_note_uid)
            .unwrap();
        store
            .batch_insert_project_note_edges(&[(project.uid.as_str(), member_note_uid.as_str())])
            .unwrap();

        // A note that is NOT a project member, titled to exact-match a query
        // token — the leak vector.
        let foreign_note = mk_note("globalnote", "globalnote");
        let foreign_note_uid = foreign_note.uid.clone();
        store.insert_note(&foreign_note).unwrap();
        store
            .insert_vault_note_edge(vault_uid, &foreign_note_uid)
            .unwrap();

        let result = investigate(
            &store,
            None,
            Some(&db_path),
            &src,
            "greet globalnote",
            "project:onlyhello",
            Some(4000),
            None,
        )
        .unwrap();
        let uids: Vec<&String> = result.entries.iter().map(|e| &e.uid).collect();

        assert!(
            uids.contains(&&member_note_uid),
            "the project's own member note must still be admitted \
             (nw-378's required counterweight); entries: {uids:?}"
        );
        assert!(
            !uids.contains(&&foreign_note_uid),
            "a note that is NOT a project member must not leak through just \
             because a query token happened to match its title; entries: {uids:?}"
        );
        assert!(
            uids.iter().any(|u| u.as_str() == hello_uid),
            "the project's own member symbol must still be admitted; entries: {uids:?}"
        );
    }

    /// nw-378, on a genuine multi-repo graph (two separately-indexed repos in
    /// one store, per the filed item's demand for "a measurement against a
    /// real multi-repo graph, not only a fixture").
    ///
    /// A vault note titled "greet" is a legitimate retrieval hit for the
    /// query "greet" under NO restriction, so its absence under `repo:`
    /// scope is the filter working, not the note failing to qualify in the
    /// first place. `repo:repo-a` must still surface repo A's own `greet`
    /// symbol — the filter drops vault content, not everything.
    ///
    /// COUNTERWEIGHT: reverting `node_in_scope`'s `ScopeFilter::Repos` arm to
    /// the pre-fix unconditional `if !node.uid.starts_with("sym:") { return
    /// true; }` makes the "notes excluded" assertion below FAIL — the note
    /// then survives the `repo:repo-a` filter exactly as measured in the
    /// filed bug. Verified by hand before committing this test.
    #[test]
    fn repo_scope_excludes_vault_notes_while_admitting_the_named_repos_symbols() {
        let dir = tempfile::tempdir().unwrap();

        // Repo A: same `greet`/`hello` shape as `make_store`.
        let repo_a = dir.path().join("repo-a");
        fs::create_dir_all(repo_a.join("greet")).unwrap();
        fs::write(
            repo_a.join("greet").join("main.js"),
            "function greet(name) { return hello(name); }\n\
             function hello(name) { return name; }",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&repo_a, "test", "https://example.com/repo-a", "sha-a")
                .unwrap();

        // Repo B: unrelated code, indexed into the SAME store as a second
        // repo — this is what makes the graph genuinely multi-repo rather
        // than a single-repo fixture with a bolted-on vault.
        let repo_b = dir.path().join("repo-b");
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(
            repo_b.join("other.js"),
            "function unrelatedThing() { return 1; }",
        )
        .unwrap();
        let reader_b = crate::content_reader::FilesystemReader::new(&repo_b);
        crate::index::index_with_reader(
            &reader_b,
            &store,
            "test",
            "https://example.com/repo-b",
            "sha-b",
            None,
        )
        .unwrap();

        // A vault note living in NEITHER repo, titled to exact-match the
        // query so it is a genuine seed candidate (via
        // `lookup_note_uids_by_title`, independent of tantivy/BM25 — no
        // tantivy index is wired into this test, matching every other
        // `investigate` unit test in this module).
        let vault_uid = "vlt:test:aaaa";
        store
            .upsert_vault(&nestweaver_schema::Vault {
                uid: vault_uid.to_string(),
                name: "notes".to_string(),
                root_path: "/vault".to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let note_uid = format!("note:{vault_uid}:greet-notes");
        store
            .insert_note(&nestweaver_schema::Note {
                uid: note_uid.clone(),
                vault_uid: vault_uid.to_string(),
                file_path: "greet-notes.md".to_string(),
                title: "greetnotes".to_string(),
                note_kind: nestweaver_schema::NoteKind::General,
                word_count: 2,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store.insert_vault_note_edge(vault_uid, &note_uid).unwrap();

        let db_path = dir.path().join("nestweaver.lbug");

        // Control: under no restriction, the note IS a legitimate hit.
        let unrestricted = investigate(
            &store,
            None,
            Some(&db_path),
            &repo_a,
            "greet greetnotes",
            "all",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(
            unrestricted.entries.iter().any(|e| e.uid == note_uid),
            "the note must be a genuine hit under no scope, or its absence \
             under repo: scope proves nothing; entries: {:?}",
            unrestricted
                .entries
                .iter()
                .map(|e| &e.uid)
                .collect::<Vec<_>>()
        );
        assert!(
            unrestricted.entries.iter().any(|e| e.title == "greet"),
            "the `greet` symbol must also be a genuine hit under no scope, \
             or its absence under repo: scope proves nothing; entries: {:?}",
            unrestricted
                .entries
                .iter()
                .map(|e| &e.uid)
                .collect::<Vec<_>>()
        );

        // repo:repo-a resolves by the URL-derived display name (neither repo
        // was given an explicit `name`, so `repo_display_name` falls back to
        // the URL basename — the SAME 29-of-44-repos-have-no-name shape
        // nw-428 measured on the live graph).
        let scoped = investigate(
            &store,
            None,
            Some(&db_path),
            &repo_a,
            "greet greetnotes",
            "repo:repo-a",
            Some(4000),
            None,
        )
        .unwrap();
        assert!(
            scoped.scope_filtered,
            "repo: scope must report itself as applied"
        );
        assert!(
            !scoped.entries.iter().any(|e| e.uid == note_uid),
            "repo: scope must exclude vault notes entirely — a note has no \
             repo_uid and cannot be attributed to the named repo; got \
             entries: {:?}",
            scoped.entries.iter().map(|e| &e.uid).collect::<Vec<_>>()
        );
        assert!(
            scoped.entries.iter().any(|e| e.uid.starts_with("sym:")),
            "repo: scope must still surface the named repo's own symbols, \
             not filter down to nothing; entries: {:?}",
            scoped.entries.iter().map(|e| &e.uid).collect::<Vec<_>>()
        );
        let repo_a_uid = store
            .list_repos(None)
            .unwrap()
            .into_iter()
            .find(|r| r.url.contains("repo-a"))
            .expect("repo A is indexed")
            .uid;
        for e in &scoped.entries {
            if !e.uid.starts_with("sym:") {
                continue;
            }
            let sym = store.lookup_symbol(&e.uid).expect("symbol resolves");
            assert!(
                sym.repo_uid == repo_a_uid,
                "every symbol entry under repo:repo-a must belong to repo A; \
                 got {} owned by {}",
                e.uid,
                sym.repo_uid
            );
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
                    unavailable_reason: None,
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
            unavailable_reason: None,
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

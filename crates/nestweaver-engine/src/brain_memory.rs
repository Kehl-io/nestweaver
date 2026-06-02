//! Feature F11: memory-bank semantics over the markdown vault.
//!
//! A typed-relationship + health + consolidation layer built on top of F9's
//! document-graph ([`crate::brain_docgraph`]) and the F11 typed Note→Note
//! edges (Supersedes / DependsOn / CausedBy / RelatesTo) derived during
//! markdown indexing.
//!
//! Three operations are exposed as `brain_memory_*` MCP tools and
//! `nestweaver memory` CLI subcommands:
//!
//! 1. [`memory_lint`] — seven health checks over the vault.
//! 2. [`memory_consolidate`] — DRY-RUN promotion proposals through the tiers
//!    (daily logs → ideas → project files). Never mutates files by default.
//! 3. [`memory_related`] — typed-edge BFS from a node, excluding the noisy
//!    generic-wikilink edges.
//!
//! Every function is graceful on an empty / no-vault DB: it returns an empty
//! result rather than erroring.
//!
//! ## Vocabulary mapping (SKOS / PROV-O)
//!
//! The four typed edges map to well-known vocabulary terms (see
//! [`nestweaver_schema::EdgeType`]):
//!
//! | Edge        | Term                  |
//! |-------------|-----------------------|
//! | Supersedes  | `prov:wasRevisionOf`  |
//! | DependsOn   | `prov:wasInformedBy`  |
//! | CausedBy    | `prov:wasDerivedFrom` |
//! | RelatesTo   | `skos:related`        |

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::Result;
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

use crate::brain_docgraph::{BrokenLink, OrphanDocument, broken_links, orphan_documents};
use crate::recency::parse_iso8601_to_epoch;

/// The four F11 typed relationship edge table names, in canonical order.
pub const TYPED_EDGE_TYPES: &[&str] = &["SUPERSEDES", "DEPENDS_ON", "CAUSED_BY", "RELATES_TO"];

/// Notes whose `modified_at` is older than this many days, while still marked
/// `status: active` in frontmatter, are flagged stale.
const STALE_AFTER_DAYS: f64 = 90.0;
const SECONDS_PER_DAY: f64 = 86_400.0;

// ── memory_related ───────────────────────────────────────────────────────────

/// A single typed neighbour reached during [`memory_related`] traversal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedNode {
    pub uid: String,
    pub title: String,
    pub file_path: String,
    /// BFS depth at which this node was first reached (1 = direct neighbour).
    pub depth: usize,
    /// The typed edge used to reach it (e.g. `SUPERSEDES`).
    pub via_edge: String,
}

/// Typed-edge breadth-first traversal from `uid` over the given `edge_types`
/// (defaults to all four when empty), out to `depth` hops (default 2 when
/// `None`). Returns the typed neighbours only — generic WIKILINK edges are
/// never traversed, so the result is free of wikilink noise. The seed node
/// itself is not included. Empty DB / unknown node → empty vec.
pub fn memory_related(
    store: &GraphStore,
    uid: &str,
    edge_types: &[String],
    depth: Option<usize>,
) -> Result<Vec<RelatedNode>> {
    let max_depth = depth.unwrap_or(2).max(1);

    // Normalise the requested edge-type filter to the canonical table names.
    let wanted: HashSet<String> = if edge_types.is_empty() {
        TYPED_EDGE_TYPES.iter().map(|s| s.to_string()).collect()
    } else {
        edge_types.iter().map(|s| normalize_edge_type(s)).collect()
    };

    let all_edges = store.typed_note_edges().map_err(|e| anyhow::anyhow!(e))?;
    if all_edges.is_empty() {
        return Ok(vec![]);
    }

    // Adjacency over the requested edge types: src → [(dst, edge_table)].
    let mut adj: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (src, dst, et) in &all_edges {
        if wanted.contains(et) {
            adj.entry(src.as_str()).or_default().push((dst, et));
        }
    }

    // BFS. Track first-reach depth + the edge used to reach each node.
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(uid);
    let mut queue: VecDeque<(&str, usize)> = VecDeque::new();
    queue.push_back((uid, 0));
    let mut reached: Vec<(String, usize, String)> = Vec::new();

    while let Some((node, d)) = queue.pop_front() {
        if d >= max_depth {
            continue;
        }
        let Some(neighbors) = adj.get(node) else {
            continue;
        };
        for (dst, et) in neighbors {
            if visited.insert(dst) {
                reached.push((dst.to_string(), d + 1, et.to_string()));
                queue.push_back((dst, d + 1));
            }
        }
    }

    if reached.is_empty() {
        return Ok(vec![]);
    }

    // Hydrate titles/paths from the note list.
    let notes = store
        .list_notes_lite(None)
        .map_err(|e| anyhow::anyhow!(e))?;
    let meta: HashMap<&str, (&str, &str)> = notes
        .iter()
        .map(|n| (n.uid.as_str(), (n.title.as_str(), n.file_path.as_str())))
        .collect();

    let mut out: Vec<RelatedNode> = reached
        .into_iter()
        .map(|(uid, depth, via_edge)| {
            let (title, file_path) = meta
                .get(uid.as_str())
                .map(|(t, p)| (t.to_string(), p.to_string()))
                .unwrap_or_default();
            RelatedNode {
                uid,
                title,
                file_path,
                depth,
                via_edge,
            }
        })
        .collect();
    out.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.uid.cmp(&b.uid)));
    Ok(out)
}

/// Normalise a user-supplied edge-type string to a canonical table name.
/// Accepts `Supersedes`, `supersedes`, `SUPERSEDES`, `depends_on`,
/// `depends-on`, `relates_to`, `caused_by`, etc.
fn normalize_edge_type(s: &str) -> String {
    let canon = s.trim().to_uppercase().replace(['-', ' '], "_");
    match canon.as_str() {
        "SUPERSEDES" => "SUPERSEDES",
        "DEPENDSON" | "DEPENDS_ON" | "DEPENDS" => "DEPENDS_ON",
        "CAUSEDBY" | "CAUSED_BY" => "CAUSED_BY",
        "RELATESTO" | "RELATES_TO" | "RELATED" => "RELATES_TO",
        other => return other.to_string(),
    }
    .to_string()
}

// ── memory_lint ────────────────────────────────────────────────────────────

/// A note flagged as stale: `status: active` but not modified within
/// [`STALE_AFTER_DAYS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleNote {
    pub uid: String,
    pub title: String,
    pub file_path: String,
    pub modified_at: Option<String>,
    pub days_stale: u64,
}

/// A Supersedes cycle (A→B→…→A). `cycle` lists the note UIDs in order; the
/// first UID is repeated implicitly as the closing node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contradiction {
    pub cycle: Vec<String>,
}

/// A supersession chain where the superseded note is still actively linked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupersessionChain {
    /// The note that supersedes (the newer one).
    pub newer_uid: String,
    /// The superseded note (the older one) that is still referenced.
    pub older_uid: String,
    /// Notes that still wikilink to the superseded `older_uid`.
    pub still_linked_from: Vec<String>,
}

/// A note whose frontmatter keys don't match its kind's template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDrift {
    pub uid: String,
    pub file_path: String,
    pub note_kind: String,
    /// Template frontmatter keys that the note is missing.
    pub missing_keys: Vec<String>,
}

/// A typed edge whose target note no longer exists in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanglingRelationship {
    pub source_uid: String,
    pub target_uid: String,
    pub edge_type: String,
}

/// Result of [`memory_lint`] — all seven keys are always present (graceful,
/// empty collections on a no-vault DB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLintReport {
    pub stale: Vec<StaleNote>,
    pub contradictions: Vec<Contradiction>,
    pub orphans: Vec<OrphanDocument>,
    pub broken_wikilinks: Vec<BrokenLink>,
    pub supersession_chains: Vec<SupersessionChain>,
    pub schema_drift: Vec<SchemaDrift>,
    pub dangling_relationships: Vec<DanglingRelationship>,
}

/// Run all seven F11 memory-bank health checks. `now_epoch` is the reference
/// time (Unix seconds) for staleness — callers pass the current wall clock;
/// tests pass a fixed value for determinism.
pub fn memory_lint(store: &GraphStore, now_epoch: f64) -> Result<MemoryLintReport> {
    let notes = store.list_notes(None).map_err(|e| anyhow::anyhow!(e))?;

    // Graceful empty: no notes ⇒ empty report (but all keys present).
    if notes.is_empty() {
        return Ok(MemoryLintReport {
            stale: vec![],
            contradictions: vec![],
            orphans: vec![],
            broken_wikilinks: vec![],
            supersession_chains: vec![],
            schema_drift: vec![],
            dangling_relationships: vec![],
        });
    }

    let uid_set: HashSet<&str> = notes.iter().map(|n| n.uid.as_str()).collect();
    let typed_edges = store.typed_note_edges().map_err(|e| anyhow::anyhow!(e))?;

    // 1. stale — status: active AND modified_at older than 90 days.
    let mut stale = Vec::new();
    for n in &notes {
        if note_status(n).as_deref() != Some("active") {
            continue;
        }
        let Some(modified) = n.modified_at.as_deref() else {
            continue;
        };
        let mtime = parse_iso8601_to_epoch(modified);
        if mtime <= 0.0 {
            continue;
        }
        let days = (now_epoch - mtime) / SECONDS_PER_DAY;
        if days > STALE_AFTER_DAYS {
            stale.push(StaleNote {
                uid: n.uid.clone(),
                title: n.title.clone(),
                file_path: n.file_path.clone(),
                modified_at: n.modified_at.clone(),
                days_stale: days.floor().max(0.0) as u64,
            });
        }
    }
    stale.sort_by(|a, b| b.days_stale.cmp(&a.days_stale).then(a.uid.cmp(&b.uid)));

    // 2. contradictions — cycles in the SUPERSEDES subgraph.
    let supersedes: Vec<(&str, &str)> = typed_edges
        .iter()
        .filter(|(_, _, et)| et == "SUPERSEDES")
        .map(|(s, t, _)| (s.as_str(), t.as_str()))
        .collect();
    let contradictions = supersedes_cycles(&supersedes);

    // 3. orphans — reuse F9.
    let orphans = orphan_documents(store, None, None, &[])?;

    // 4. broken_wikilinks — reuse F9.
    let broken_wikilinks = broken_links(store, 5)?;

    // 5. supersession_chains — A supersedes B where B is still wikilinked
    //    from notes.
    let wikilink_edges = store
        .note_wikilink_edges()
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut inbound: HashMap<&str, Vec<&str>> = HashMap::new();
    for (src, dst) in &wikilink_edges {
        inbound.entry(dst.as_str()).or_default().push(src.as_str());
    }
    let mut supersession_chains = Vec::new();
    for (newer, older) in &supersedes {
        if let Some(linkers) = inbound.get(older) {
            // Don't count the superseding note's own link to the older one.
            let still: Vec<String> = linkers
                .iter()
                .filter(|l| **l != *newer)
                .map(|l| l.to_string())
                .collect();
            if !still.is_empty() {
                supersession_chains.push(SupersessionChain {
                    newer_uid: newer.to_string(),
                    older_uid: older.to_string(),
                    still_linked_from: still,
                });
            }
        }
    }
    supersession_chains.sort_by(|a, b| {
        a.newer_uid
            .cmp(&b.newer_uid)
            .then(a.older_uid.cmp(&b.older_uid))
    });

    // 6. schema_drift — frontmatter keys vs _templates/<note_kind>.md.
    let templates = load_templates(store);
    let mut schema_drift = Vec::new();
    if !templates.is_empty() {
        for n in &notes {
            let kind_key = n.note_kind.to_string().to_lowercase();
            let Some(required) = templates.get(&kind_key) else {
                continue;
            };
            let present = note_frontmatter_keys(n);
            let missing: Vec<String> = required
                .iter()
                .filter(|k| !present.contains(*k))
                .cloned()
                .collect();
            if !missing.is_empty() {
                schema_drift.push(SchemaDrift {
                    uid: n.uid.clone(),
                    file_path: n.file_path.clone(),
                    note_kind: n.note_kind.to_string(),
                    missing_keys: missing,
                });
            }
        }
        schema_drift.sort_by(|a, b| a.uid.cmp(&b.uid));
    }

    // 7. dangling_relationships — a DECLARED typed edge whose target doesn't
    //    exist. The graph store physically cannot hold an edge to a missing
    //    node (LadybugDB requires both endpoints; `DETACH DELETE` removes
    //    incident edges), so we detect dangling intent at its true source:
    //    a frontmatter relationship key (`supersedes:` / `depends_on:` /
    //    `caused_by:` / `relates_to:`) whose referenced note can't be resolved
    //    against the current vault. We also defensively check the physical
    //    typed edges (always empty today, but future-proof).
    let mut dangling_relationships = Vec::new();
    for (src, dst, et) in &typed_edges {
        if !uid_set.contains(dst.as_str()) {
            dangling_relationships.push(DanglingRelationship {
                source_uid: src.clone(),
                target_uid: dst.clone(),
                edge_type: et.clone(),
            });
        }
    }
    // Build a title→uid index + the uid set to resolve frontmatter references.
    let mut by_title: HashMap<String, Vec<&str>> = HashMap::new();
    for n in &notes {
        by_title
            .entry(n.title.to_lowercase())
            .or_default()
            .push(n.uid.as_str());
    }
    for n in &notes {
        let Some(fm) = n.frontmatter.as_deref() else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(fm) else {
            continue;
        };
        for (key, et) in [
            ("supersedes", "SUPERSEDES"),
            ("depends_on", "DEPENDS_ON"),
            ("caused_by", "CAUSED_BY"),
            ("relates_to", "RELATES_TO"),
        ] {
            for reference in frontmatter_refs(&json, key) {
                let resolved = resolve_reference(&reference, &uid_set, &by_title);
                if resolved.is_none() {
                    dangling_relationships.push(DanglingRelationship {
                        source_uid: n.uid.clone(),
                        target_uid: reference,
                        edge_type: et.to_string(),
                    });
                }
            }
        }
    }
    dangling_relationships.sort_by(|a, b| {
        a.source_uid
            .cmp(&b.source_uid)
            .then(a.target_uid.cmp(&b.target_uid))
    });

    Ok(MemoryLintReport {
        stale,
        contradictions,
        orphans,
        broken_wikilinks,
        supersession_chains,
        schema_drift,
        dangling_relationships,
    })
}

/// Extract a frontmatter relationship key as a list of reference strings
/// (array form `key: [A, B]` or scalar `key: A`). Empty when absent.
fn frontmatter_refs(json: &serde_json::Value, key: &str) -> Vec<String> {
    match json.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                vec![]
            } else {
                vec![t.to_string()]
            }
        }
        _ => vec![],
    }
}

/// Resolve a frontmatter reference (note UID or title) against the vault.
/// Returns `None` when it matches no note (a dangling relationship).
fn resolve_reference(
    reference: &str,
    uid_set: &HashSet<&str>,
    by_title: &HashMap<String, Vec<&str>>,
) -> Option<String> {
    let raw = reference.trim();
    if uid_set.contains(raw) {
        return Some(raw.to_string());
    }
    by_title
        .get(&raw.to_lowercase())
        .and_then(|uids| uids.first().map(|u| u.to_string()))
}

/// Read frontmatter `status` (lowercased) from a note, if present.
fn note_status(note: &nestweaver_schema::Note) -> Option<String> {
    let fm = note.frontmatter.as_deref()?;
    let json: serde_json::Value = serde_json::from_str(fm).ok()?;
    json.get("status")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
}

/// All top-level frontmatter keys of a note (empty when no frontmatter).
fn note_frontmatter_keys(note: &nestweaver_schema::Note) -> HashSet<String> {
    let Some(fm) = note.frontmatter.as_deref() else {
        return HashSet::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(fm) else {
        return HashSet::new();
    };
    json.as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Find cycles in the Supersedes subgraph. Returns each elementary cycle once.
/// For F11 the common case is the 2-cycle A→B→A; longer cycles are reported
/// in the order discovered. Each [`Contradiction`] lists the cycle's nodes
/// once (the closing edge back to the first is implicit).
fn supersedes_cycles(edges: &[(&str, &str)]) -> Vec<Contradiction> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (s, t) in edges {
        adj.entry(s).or_default().push(t);
    }

    let mut cycles: Vec<Contradiction> = Vec::new();
    let mut seen_signatures: HashSet<Vec<String>> = HashSet::new();
    let mut color: HashMap<&str, u8> = HashMap::new(); // 0=white,1=grey,2=black

    // Iterative DFS with an explicit path stack so we can recover the cycle.
    for &start in adj.keys() {
        if color.get(start).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut path: Vec<&str> = Vec::new();
        dfs_cycles(
            start,
            &adj,
            &mut color,
            &mut path,
            &mut cycles,
            &mut seen_signatures,
        );
    }
    cycles
}

fn dfs_cycles<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    color: &mut HashMap<&'a str, u8>,
    path: &mut Vec<&'a str>,
    cycles: &mut Vec<Contradiction>,
    seen: &mut HashSet<Vec<String>>,
) {
    color.insert(node, 1);
    path.push(node);
    if let Some(neighbors) = adj.get(node) {
        for &next in neighbors {
            match color.get(next).copied().unwrap_or(0) {
                1 => {
                    // Back-edge → cycle. Slice the path from `next` onward.
                    if let Some(pos) = path.iter().position(|&n| n == next) {
                        let cycle: Vec<String> =
                            path[pos..].iter().map(|s| s.to_string()).collect();
                        let mut sig = cycle.clone();
                        sig.sort();
                        if seen.insert(sig) {
                            cycles.push(Contradiction { cycle });
                        }
                    }
                }
                0 => dfs_cycles(next, adj, color, path, cycles, seen),
                _ => {}
            }
        }
    }
    path.pop();
    color.insert(node, 2);
}

/// Load template frontmatter keys from `_templates/<kind>.md` notes in the
/// vault. Returns `note_kind_lowercased → {required keys}`. Empty when no
/// template notes exist. A template note's path is matched against
/// `_templates/` (case-insensitive) and its stem is the note-kind hint.
fn load_templates(store: &GraphStore) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let Ok(notes) = store.list_notes(None) else {
        return out;
    };
    for n in &notes {
        let path_lc = n.file_path.to_lowercase().replace('\\', "/");
        if !path_lc.contains("_templates/") {
            continue;
        }
        let Some(stem) = std::path::Path::new(&path_lc)
            .file_stem()
            .and_then(|s| s.to_str())
        else {
            continue;
        };
        // Map the template stem through the same kind-hint logic the parser
        // uses, so `_templates/meeting.md` governs Meeting notes.
        let kind = nestweaver_schema::NoteKind::from_hint(stem)
            .to_string()
            .to_lowercase();
        let keys = note_frontmatter_keys(n);
        if !keys.is_empty() {
            out.entry(kind).or_default().extend(keys);
        }
    }
    out
}

// ── memory_consolidate ───────────────────────────────────────────────────────

/// A single promotion proposal in the consolidation manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationProposal {
    pub source_uid: String,
    pub source_title: String,
    pub source_path: String,
    /// The tier the source is proposed to be promoted INTO.
    pub promote_to: String,
    /// Human-readable justification (counts, ages).
    pub rationale: String,
    /// Supporting note UIDs (referencing ideas / project files).
    pub evidence: Vec<String>,
}

/// Result of [`memory_consolidate`]. `applied` is false in the safe
/// dry-run default; set `--apply` to move files and return `applied: true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationManifest {
    pub dry_run: bool,
    pub applied: bool,
    pub proposals: Vec<ConsolidationProposal>,
    /// Warnings (e.g. that `--apply` is not yet implemented).
    pub warnings: Vec<String>,
}

/// Number of distinct idea-note referrers a daily log needs to be promoted.
const LOG_PROMOTION_MIN_REFERRERS: usize = 3;
/// A daily log must be older than this many days to be a promotion candidate.
const LOG_PROMOTION_MIN_AGE_DAYS: f64 = 14.0;

/// Propose tier promotions over the vault. DRY-RUN by default — no files are
/// ever mutated. `apply` is honoured only as an explicit, provenance-recording
/// When `apply` is true, proposed promotions are carried out: each source file
/// is moved to its target directory under the vault root, and summaries are
/// appended to `warnings`.
///
/// Rules:
/// - A daily-log note (path under `_logs/`) wikilinked from ≥3 distinct idea
///   notes (path under `_ideas/`) and older than 14 days → promote to `_ideas`.
/// - An idea note (path under `_ideas/`) referenced from a project's `sync.md`
///   AND its `status.md` → promote to a project file.
pub fn memory_consolidate(
    store: &GraphStore,
    apply: bool,
    now_epoch: f64,
) -> Result<ConsolidationManifest> {
    let notes = store.list_notes(None).map_err(|e| anyhow::anyhow!(e))?;
    let mut warnings = Vec::new();
    if notes.is_empty() {
        return Ok(ConsolidationManifest {
            dry_run: !apply,
            applied: false,
            proposals: vec![],
            warnings,
        });
    }

    let meta: HashMap<&str, &nestweaver_schema::Note> =
        notes.iter().map(|n| (n.uid.as_str(), n)).collect();
    let path_of = |uid: &str| meta.get(uid).map(|n| n.file_path.to_lowercase());

    let wikilink_edges = store
        .note_wikilink_edges()
        .map_err(|e| anyhow::anyhow!(e))?;
    // inbound[dst] = distinct source uids that wikilink to dst.
    let mut inbound: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (src, dst) in &wikilink_edges {
        inbound
            .entry(dst.as_str())
            .or_default()
            .insert(src.as_str());
    }

    let mut proposals = Vec::new();

    for n in &notes {
        let path_lc = n.file_path.to_lowercase().replace('\\', "/");
        let linkers = inbound.get(n.uid.as_str());

        // Rule 1: daily log → _ideas.
        if is_in_dir(&path_lc, "_logs") {
            let idea_referrers: Vec<&str> = linkers
                .map(|set| {
                    set.iter()
                        .copied()
                        .filter(|u| path_of(u).is_some_and(|p| is_in_dir(&p, "_ideas")))
                        .collect()
                })
                .unwrap_or_default();
            let age_days = n
                .modified_at
                .as_deref()
                .or(n.created_at.as_deref())
                .map(|t| (now_epoch - parse_iso8601_to_epoch(t)) / SECONDS_PER_DAY)
                .unwrap_or(0.0);
            if idea_referrers.len() >= LOG_PROMOTION_MIN_REFERRERS
                && age_days > LOG_PROMOTION_MIN_AGE_DAYS
            {
                let mut evidence: Vec<String> =
                    idea_referrers.iter().map(|s| s.to_string()).collect();
                evidence.sort();
                proposals.push(ConsolidationProposal {
                    source_uid: n.uid.clone(),
                    source_title: n.title.clone(),
                    source_path: n.file_path.clone(),
                    promote_to: "_ideas".to_string(),
                    rationale: format!(
                        "daily log linked from {} idea note(s) and {} days old (>{} day threshold)",
                        idea_referrers.len(),
                        age_days.floor() as u64,
                        LOG_PROMOTION_MIN_AGE_DAYS as u64,
                    ),
                    evidence,
                });
            }
        }

        // Rule 2: idea referenced from a project's sync.md AND status.md.
        if is_in_dir(&path_lc, "_ideas")
            && let Some(set) = linkers
        {
            // Group referrers by project dir, requiring both sync.md + status.md.
            let mut by_project: HashMap<String, HashSet<&str>> = HashMap::new();
            for &referrer in set {
                if let Some(rpath) = path_of(referrer) {
                    let file = std::path::Path::new(&rpath)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if file == "sync.md" || file == "status.md" {
                        let proj = std::path::Path::new(&rpath)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        by_project.entry(proj).or_default().insert(file_label(file));
                    }
                }
            }
            for (proj, files) in &by_project {
                if files.contains("sync") && files.contains("status") {
                    let mut evidence: Vec<String> = set
                        .iter()
                        .filter(|u| path_of(u).is_some_and(|p| p.starts_with(proj)))
                        .map(|s| s.to_string())
                        .collect();
                    evidence.sort();
                    proposals.push(ConsolidationProposal {
                        source_uid: n.uid.clone(),
                        source_title: n.title.clone(),
                        source_path: n.file_path.clone(),
                        promote_to: format!("project-file ({proj})"),
                        rationale: format!(
                            "idea referenced from both sync.md and status.md of project '{proj}'"
                        ),
                        evidence,
                    });
                }
            }
        }
    }

    proposals.sort_by(|a, b| {
        a.source_uid
            .cmp(&b.source_uid)
            .then(a.promote_to.cmp(&b.promote_to))
    });

    if apply && !proposals.is_empty() {
        match apply_proposals(store, &proposals, &notes) {
            Ok((success, summaries)) => {
                warnings.extend(summaries);
                if success {
                    warnings.push(
                        "Re-index the vault to update the graph: \
                         nestweaver brain refresh <vault-path>"
                            .to_string(),
                    );
                }
                return Ok(ConsolidationManifest {
                    dry_run: false,
                    applied: success,
                    proposals,
                    warnings,
                });
            }
            Err(e) => {
                warnings.push(format!("apply failed: {e}"));
                return Ok(ConsolidationManifest {
                    dry_run: false,
                    applied: false,
                    proposals,
                    warnings,
                });
            }
        }
    }

    Ok(ConsolidationManifest {
        dry_run: !apply,
        applied: false,
        proposals,
        warnings,
    })
}

/// Execute the file moves described by `proposals`, returning
/// `(all_succeeded, summaries)`.
fn apply_proposals(
    store: &GraphStore,
    proposals: &[ConsolidationProposal],
    notes: &[nestweaver_schema::Note],
) -> Result<(bool, Vec<String>)> {
    let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;

    let vault_roots: HashMap<&str, &str> = vaults
        .iter()
        .map(|v| (v.uid.as_str(), v.root_path.as_str()))
        .collect();

    let note_by_uid: HashMap<&str, &nestweaver_schema::Note> =
        notes.iter().map(|n| (n.uid.as_str(), n)).collect();

    let mut summaries = Vec::new();
    let mut all_ok = true;

    for p in proposals {
        // Resolve the vault root for this note.
        let note = match note_by_uid.get(p.source_uid.as_str()) {
            Some(n) => *n,
            None => {
                summaries.push(format!(
                    "SKIP: note uid '{}' not found in store",
                    p.source_uid
                ));
                all_ok = false;
                continue;
            }
        };
        let vault_root = match vault_roots.get(note.vault_uid.as_str()) {
            Some(r) => std::path::Path::new(*r),
            None => {
                summaries.push(format!(
                    "SKIP: vault uid '{}' not found for note '{}'",
                    note.vault_uid, p.source_path
                ));
                all_ok = false;
                continue;
            }
        };

        let src = vault_root.join(&p.source_path);
        if !src.exists() {
            summaries.push(format!("SKIP: source does not exist: {}", src.display()));
            all_ok = false;
            continue;
        }

        // Determine the destination path.
        let file_name = match src.file_name() {
            Some(f) => f,
            None => {
                summaries.push(format!(
                    "SKIP: cannot extract filename from '{}'",
                    src.display()
                ));
                all_ok = false;
                continue;
            }
        };

        let dest_dir = if p.promote_to == "_ideas" {
            vault_root.join("_ideas")
        } else if p.promote_to.starts_with("project-file (") {
            // Extract the project dir from "project-file (<dir>)".
            let inner = p
                .promote_to
                .strip_prefix("project-file (")
                .and_then(|s| s.strip_suffix(')'));
            match inner {
                Some(proj_dir) => vault_root.join(proj_dir),
                None => {
                    summaries.push(format!(
                        "SKIP: cannot parse project dir from promote_to '{}'",
                        p.promote_to
                    ));
                    all_ok = false;
                    continue;
                }
            }
        } else {
            summaries.push(format!(
                "SKIP: unknown promote_to target '{}'",
                p.promote_to
            ));
            all_ok = false;
            continue;
        };

        let dest = dest_dir.join(file_name);

        if dest.exists() {
            summaries.push(format!(
                "SKIP: destination already exists: {}",
                dest.display()
            ));
            all_ok = false;
            continue;
        }

        // Ensure destination directory exists.
        if let Err(e) = std::fs::create_dir_all(&dest_dir) {
            summaries.push(format!(
                "SKIP: cannot create dir '{}': {e}",
                dest_dir.display()
            ));
            all_ok = false;
            continue;
        }

        // Move the file: try rename first, fall back to copy + delete for
        // cross-filesystem moves.
        let moved = match std::fs::rename(&src, &dest) {
            Ok(()) => true,
            Err(_rename_err) => match std::fs::copy(&src, &dest) {
                Ok(_) => {
                    if let Err(e) = std::fs::remove_file(&src) {
                        summaries.push(format!(
                            "WARN: copied to '{}' but failed to remove source '{}': {e}",
                            dest.display(),
                            src.display(),
                        ));
                    }
                    true
                }
                Err(e) => {
                    summaries.push(format!(
                        "SKIP: failed to move '{}' → '{}': {e}",
                        src.display(),
                        dest.display(),
                    ));
                    all_ok = false;
                    false
                }
            },
        };

        if moved {
            summaries.push(format!("MOVED: {} → {}", src.display(), dest.display()));

            // Rewrite path-based wikilinks in notes that reference this file.
            let old_stem = Path::new(&p.source_path)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let new_rel = dest
                .strip_prefix(vault_root)
                .unwrap_or(&dest)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");

            if old_stem != new_rel {
                let rewrite_count =
                    rewrite_wikilinks(vault_root, &note_by_uid, &p.evidence, &old_stem, &new_rel);
                if rewrite_count > 0 {
                    summaries.push(format!(
                        "  REWRITE: updated [[{old_stem}]] → [[{new_rel}]] in {rewrite_count} file(s)"
                    ));
                }
            }
        }
    }

    Ok((all_ok, summaries))
}

/// Rewrite `[[old_stem]]` → `[[new_stem]]` in the files identified by `evidence_uids`.
/// Returns the number of files actually modified.
fn rewrite_wikilinks(
    vault_root: &Path,
    note_by_uid: &HashMap<&str, &nestweaver_schema::Note>,
    evidence_uids: &[String],
    old_stem: &str,
    new_stem: &str,
) -> usize {
    let old_link = format!("[[{old_stem}]]");
    let new_link = format!("[[{new_stem}]]");
    // Also handle display-text links: [[path|display]]
    let old_link_prefix = format!("[[{old_stem}|");
    let new_link_prefix = format!("[[{new_stem}|");

    let mut count = 0;
    for uid in evidence_uids {
        let Some(note) = note_by_uid.get(uid.as_str()) else {
            continue;
        };
        let path = vault_root.join(&note.file_path);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let updated = content
            .replace(&old_link, &new_link)
            .replace(&old_link_prefix, &new_link_prefix);
        if updated != content && std::fs::write(&path, &updated).is_ok() {
            count += 1;
        }
    }
    count
}

/// True when `path_lc` (lowercased, forward-slash) is inside a directory named
/// `dir` at any depth (`<dir>/…`).
fn is_in_dir(path_lc: &str, dir: &str) -> bool {
    let needle = format!("{dir}/");
    path_lc.starts_with(&needle) || path_lc.contains(&format!("/{needle}"))
}

/// Map a project filename to its short label (`sync.md` → `sync`).
fn file_label(file: &str) -> &'static str {
    match file {
        "sync.md" => "sync",
        "status.md" => "status",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_md::index_markdown_directory_in_memory;
    use std::fs;

    fn make_vault(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        (dir, root)
    }

    /// A small vault exercising the F11 surface:
    /// - Alpha `supersedes: [Beta]` in frontmatter → Supersedes edge.
    /// - Alpha `depends_on: [Gamma]` → DependsOn edge.
    /// - Delta has a "See also" section linking [[Gamma]] → RelatesTo.
    /// - Delta also has an ungrouped [[Alpha]] link → stays generic WIKILINK.
    /// - Cycle: Cyc1 supersedes Cyc2, Cyc2 supersedes Cyc1 → contradiction.
    /// - Stale: an `status: active` note with an ancient modified_at frontmatter
    ///   is detected via frontmatter `modified_at` (we inject it).
    fn f11_vault() -> (tempfile::TempDir, GraphStore) {
        let (dir, root) = make_vault(&[
            (
                "Alpha.md",
                "---\nsupersedes: [Beta]\ndepends_on: [Gamma]\n---\n# Alpha\n\nbody\n",
            ),
            ("Beta.md", "# Beta\n\nold thing\n"),
            ("Gamma.md", "# Gamma\n\na dep\n"),
            (
                "Delta.md",
                "# Delta\n\nIntro with [[Alpha]] generic link.\n\n## See also\n\n[[Gamma]]\n",
            ),
            ("Cyc1.md", "---\nsupersedes: [Cyc2]\n---\n# Cyc1\n"),
            ("Cyc2.md", "---\nsupersedes: [Cyc1]\n---\n# Cyc2\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        (dir, store)
    }

    fn uid_for(store: &GraphStore, title: &str) -> String {
        store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .find(|n| n.title == title)
            .unwrap_or_else(|| panic!("note titled {title} not found"))
            .uid
    }

    #[test]
    fn typed_edges_are_derived_from_frontmatter_and_headings() {
        let (_dir, store) = f11_vault();
        let edges = store.typed_note_edges().unwrap();
        let alpha = uid_for(&store, "Alpha");
        let beta = uid_for(&store, "Beta");
        let gamma = uid_for(&store, "Gamma");
        let delta = uid_for(&store, "Delta");

        // Frontmatter supersedes → Supersedes.
        assert!(
            edges
                .iter()
                .any(|(s, t, et)| s == &alpha && t == &beta && et == "SUPERSEDES"),
            "expected Alpha SUPERSEDES Beta"
        );
        // Frontmatter depends_on → DependsOn.
        assert!(
            edges
                .iter()
                .any(|(s, t, et)| s == &alpha && t == &gamma && et == "DEPENDS_ON"),
            "expected Alpha DEPENDS_ON Gamma"
        );
        // "See also" heading group → RelatesTo.
        assert!(
            edges
                .iter()
                .any(|(s, t, et)| s == &delta && t == &gamma && et == "RELATES_TO"),
            "expected Delta RELATES_TO Gamma (See also section)"
        );
        // The ungrouped [[Alpha]] in Delta's intro must NOT become a typed edge.
        assert!(
            !edges.iter().any(|(s, t, _)| s == &delta && t == &alpha),
            "ungrouped wikilink Delta→Alpha must stay generic, not typed"
        );
    }

    #[test]
    fn no_regression_on_generic_wikilinks() {
        let (_dir, store) = f11_vault();
        // Delta's intro link to Alpha is a normal wikilink and must survive.
        let wl = store.note_wikilink_edges().unwrap();
        let alpha = uid_for(&store, "Alpha");
        let delta = uid_for(&store, "Delta");
        assert!(
            wl.iter().any(|(s, t)| s == &delta && t == &alpha),
            "generic wikilink Delta→Alpha must still be present"
        );
    }

    #[test]
    fn memory_related_returns_only_typed_neighbours() {
        let (_dir, store) = f11_vault();
        let alpha = uid_for(&store, "Alpha");
        let beta = uid_for(&store, "Beta");
        let gamma = uid_for(&store, "Gamma");

        let related = memory_related(&store, &alpha, &[], None).unwrap();
        let reached: HashSet<&str> = related.iter().map(|r| r.uid.as_str()).collect();
        // Alpha supersedes Beta and depends_on Gamma → both typed neighbours.
        assert!(reached.contains(beta.as_str()), "Beta is a typed neighbour");
        assert!(
            reached.contains(gamma.as_str()),
            "Gamma is a typed neighbour"
        );

        // Filter to SUPERSEDES only → only Beta.
        let only_sup =
            memory_related(&store, &alpha, &["supersedes".to_string()], Some(2)).unwrap();
        let sup_reached: HashSet<&str> = only_sup.iter().map(|r| r.uid.as_str()).collect();
        assert!(sup_reached.contains(beta.as_str()));
        assert!(
            !sup_reached.contains(gamma.as_str()),
            "DependsOn neighbour must be excluded when filtering to Supersedes"
        );
    }

    #[test]
    fn memory_related_excludes_generic_wikilinks() {
        let (_dir, store) = f11_vault();
        // Delta links to Alpha only via a generic wikilink (no typed edge).
        let delta = uid_for(&store, "Delta");
        let alpha = uid_for(&store, "Alpha");
        let related = memory_related(&store, &delta, &[], Some(2)).unwrap();
        let reached: HashSet<&str> = related.iter().map(|r| r.uid.as_str()).collect();
        assert!(
            !reached.contains(alpha.as_str()),
            "memory_related must not traverse the generic Delta→Alpha wikilink"
        );
        // But Delta→Gamma (RelatesTo, from "See also") IS typed.
        let gamma = uid_for(&store, "Gamma");
        assert!(
            reached.contains(gamma.as_str()),
            "RelatesTo neighbour expected"
        );
    }

    #[test]
    fn lint_surfaces_contradiction_cycle() {
        let (_dir, store) = f11_vault();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        assert!(
            !report.contradictions.is_empty(),
            "expected the Cyc1↔Cyc2 Supersedes cycle to be flagged"
        );
        let cyc1 = uid_for(&store, "Cyc1");
        let cyc2 = uid_for(&store, "Cyc2");
        let found = report
            .contradictions
            .iter()
            .any(|c| c.cycle.contains(&cyc1) && c.cycle.contains(&cyc2));
        assert!(found, "the cycle should contain both Cyc1 and Cyc2");
    }

    #[test]
    fn lint_surfaces_stale_active_note() {
        // A note with status: active and an ancient modified_at frontmatter.
        // We use frontmatter modified_at because filesystem mtime is "now".
        // The store reads modified_at from file metadata, so to test staleness
        // deterministically we index then assert via a synthetic old timestamp:
        // build a vault whose note carries status: active, then lint with a
        // now_epoch far in the future so the (recent) file mtime is > 90 days.
        let (_dir, root) = make_vault(&[(
            "Active.md",
            "---\nstatus: active\n---\n# Active\n\nstill open\n",
        )]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        // now = file mtime + 200 days.
        let note = store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .find(|n| n.title == "Active")
            .unwrap();
        let mtime = parse_iso8601_to_epoch(note.modified_at.as_deref().unwrap_or(""));
        assert!(mtime > 0.0, "note should have a modified_at timestamp");
        let future = mtime + 200.0 * SECONDS_PER_DAY;
        let report = memory_lint(&store, future).unwrap();
        assert!(
            report.stale.iter().any(|s| s.title == "Active"),
            "active note older than 90 days must be flagged stale"
        );
    }

    #[test]
    fn lint_detects_schema_drift_against_template() {
        // _templates/meeting.md defines the expected frontmatter keys; a
        // Meeting note missing some of them drifts.
        let (_dir, root) = make_vault(&[
            (
                "_templates/meeting.md",
                "---\ndate: \nattendees: \naction_items: \n---\n# Meeting Template\n",
            ),
            (
                "standup.md",
                "---\ntype: meeting\ndate: 2026-01-01\n---\n# Standup\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        let drift = report
            .schema_drift
            .iter()
            .find(|d| d.file_path.ends_with("standup.md"));
        assert!(
            drift.is_some(),
            "standup.md should drift from the meeting template"
        );
        let drift = drift.unwrap();
        assert!(
            drift.missing_keys.iter().any(|k| k == "attendees")
                && drift.missing_keys.iter().any(|k| k == "action_items"),
            "missing_keys should include attendees and action_items, got {:?}",
            drift.missing_keys
        );
    }

    #[test]
    fn lint_detects_dangling_relationship() {
        // A frontmatter relationship whose target note does not exist in the
        // vault is a dangling relationship. (The graph store cannot physically
        // hold an edge to a missing node, so detection works off the declared
        // frontmatter intent.)
        let (_dir, root) = make_vault(&[(
            "Orphaned.md",
            "---\ndepends_on: [Nonexistent Note]\n---\n# Orphaned\n",
        )]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        assert!(
            report
                .dangling_relationships
                .iter()
                .any(|d| d.target_uid == "Nonexistent Note" && d.edge_type == "DEPENDS_ON"),
            "expected a dangling relationship for the missing target, got {:?}",
            report.dangling_relationships
        );
    }

    #[test]
    fn consolidate_is_dry_run_by_default() {
        // A daily log linked from 3 idea notes, older than 14 days.
        let (_dir, root) = make_vault(&[
            (
                "_logs/2025-01-01.md",
                "# Log Jan 1\n\nA recurring idea worth promoting.\n",
            ),
            (
                "_ideas/idea-a.md",
                "# Idea A\n\nSee [[_logs/2025-01-01]].\n",
            ),
            (
                "_ideas/idea-b.md",
                "# Idea B\n\nRefs [[_logs/2025-01-01]].\n",
            ),
            (
                "_ideas/idea-c.md",
                "# Idea C\n\nAlso [[_logs/2025-01-01]].\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        // now far in the future so the log is "old".
        let manifest = memory_consolidate(&store, false, 4_000_000_000.0).unwrap();
        assert!(manifest.dry_run, "default must be dry-run");
        assert!(!manifest.applied, "dry-run never applies");
        let promoted = manifest
            .proposals
            .iter()
            .find(|p| p.source_path.ends_with("2025-01-01.md"));
        assert!(
            promoted.is_some(),
            "the well-referenced old daily log should be an _ideas candidate"
        );
        assert_eq!(promoted.unwrap().promote_to, "_ideas");
    }

    #[test]
    fn consolidate_apply_on_empty_is_noop() {
        let store = GraphStore::in_memory().unwrap();
        let manifest = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();
        assert!(!manifest.applied);
        assert!(manifest.proposals.is_empty());
    }

    #[test]
    fn consolidate_apply_moves_log_to_ideas() {
        let (_dir, root) = make_vault(&[
            (
                "_logs/2025-01-01.md",
                "# Log Jan 1\n\nA recurring idea worth promoting.\n",
            ),
            (
                "_ideas/idea-a.md",
                "# Idea A\n\nSee [[_logs/2025-01-01]].\n",
            ),
            (
                "_ideas/idea-b.md",
                "# Idea B\n\nRefs [[_logs/2025-01-01]].\n",
            ),
            (
                "_ideas/idea-c.md",
                "# Idea C\n\nAlso [[_logs/2025-01-01]].\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let manifest = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();

        assert!(!manifest.dry_run);
        assert!(
            !manifest.proposals.is_empty(),
            "should have at least one proposal"
        );

        assert!(
            manifest.applied,
            "apply should have succeeded; warnings: {:?}",
            manifest.warnings
        );

        let moved = root.join("_ideas/2025-01-01.md");
        assert!(moved.exists(), "file should exist in _ideas/ after apply");
        assert!(
            !root.join("_logs/2025-01-01.md").exists(),
            "original should be gone"
        );

        assert!(
            manifest
                .warnings
                .iter()
                .any(|w| w.contains("re-index") || w.contains("Re-index")),
            "should warn user to re-index after apply"
        );

        // Wikilinks should have been rewritten from [[_logs/2025-01-01]] to [[_ideas/2025-01-01]]
        let idea_a = fs::read_to_string(root.join("_ideas/idea-a.md")).unwrap();
        assert!(
            idea_a.contains("[[_ideas/2025-01-01]]"),
            "idea-a.md should have updated wikilink, got: {idea_a}"
        );
        assert!(
            !idea_a.contains("[[_logs/2025-01-01]]"),
            "idea-a.md should not have old wikilink"
        );
    }

    #[test]
    fn empty_db_is_graceful() {
        let store = GraphStore::in_memory().unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        assert!(report.stale.is_empty());
        assert!(report.contradictions.is_empty());
        assert!(report.orphans.is_empty());
        assert!(report.dangling_relationships.is_empty());

        assert!(
            memory_related(&store, "nope", &[], None)
                .unwrap()
                .is_empty()
        );

        let manifest = memory_consolidate(&store, false, 0.0).unwrap();
        assert!(manifest.proposals.is_empty());
        assert!(manifest.dry_run);
    }
}

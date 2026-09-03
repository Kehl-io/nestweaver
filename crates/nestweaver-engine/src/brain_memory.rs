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
use std::io::Write;
use std::path::{Component, Path, PathBuf};

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
            // A template defines the schema; it cannot drift from it. The
            // templates are themselves notes, so without this every template
            // was linted against the merged bucket and reported as drifting
            // from every other template (nw-307).
            if n.file_path
                .to_lowercase()
                .replace('\\', "/")
                .contains("_templates/")
            {
                continue;
            }
            let kind_key = note_template_key(n);
            // A note whose declared kind has no template is untemplated, not
            // drifting. This is what keeps the fix from over-flagging in the
            // other direction.
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

/// Load template frontmatter keys from `_templates/<name>.md` notes in the
/// vault. Returns `template_stem_lowercased → {required keys}`. Empty when no
/// template notes exist. A template note's path is matched against
/// `_templates/` (case-insensitive).
///
/// The key is the template's own STEM, not `NoteKind::from_hint(stem)`.
/// `NoteKind` is a six-variant retrieval hint with a `General` catch-all, so it
/// is not injective over template names: `Log`, `Architecture`, `Decision`,
/// `Backlog Item`, `Project` and `Person` all mapped to `general`, and
/// `HashSet::extend` then UNIONED their key sets into one bucket. Every note
/// that also landed on `General` was checked against that union, so a daily log
/// matching `_templates/Log.md` exactly was reported missing twelve keys that
/// belong to other templates — 96% of the vault flagged (nw-307).
///
/// The collision was lossy in the worst direction: additive, so each new
/// unrecognised template made the bucket stricter for every note in it.
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
        let keys = note_frontmatter_keys(n);
        if !keys.is_empty() {
            out.entry(normalize_template_key(stem))
                .or_default()
                .extend(keys);
        }
    }
    out
}

/// Collapse a template name or a note's declared kind to one comparable key.
///
/// Lowercased, with every run of non-alphanumeric characters reduced to a
/// single space, so `_templates/Backlog Item.md`, `type: backlog-item` and
/// `category: Backlog_Item` all name the same template.
fn normalize_template_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// The template key a note declares for ITSELF: frontmatter `type:`, then
/// `category:`, then its `note_kind` display name.
///
/// The note's own declared identity is what the schema check needs; routing it
/// through `NoteKind` is what made six distinct template names indistinguishable
/// (nw-307).
fn note_template_key(note: &nestweaver_schema::Note) -> String {
    for field in ["type", "category"] {
        if let Some(value) = note_frontmatter_string(note, field) {
            let key = normalize_template_key(&value);
            if !key.is_empty() {
                return key;
            }
        }
    }
    normalize_template_key(&note.note_kind.to_string())
}

/// Read one string-valued frontmatter field from a note's stored JSON.
fn note_frontmatter_string(note: &nestweaver_schema::Note, key: &str) -> Option<String> {
    let fm = note.frontmatter.as_deref()?;
    let json = serde_json::from_str::<serde_json::Value>(fm).ok()?;
    json.get(key)?.as_str().map(|s| s.to_string())
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
    /// Apply summaries, recovery locations, and re-index guidance.
    pub warnings: Vec<String>,
}

const CONSOLIDATION_JOURNAL_VERSION: u32 = 1;
const CONSOLIDATION_JOURNAL_DIR: &str = ".nestweaver-consolidation-journal";

/// A durable checkpoint for one promotion. Journals intentionally live in the
/// affected vault: recovering a note move must not depend on the graph DB or
/// on database-side backup/snapshot policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ConsolidationPhase {
    Prepared,
    DestinationPublished,
    SourceRemoved,
    RewritesApplied,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsolidationJournal {
    version: u32,
    journal_id: String,
    vault_uid: String,
    proposal: ConsolidationProposal,
    source_path: String,
    destination_path: String,
    source_byte_len: u64,
    source_blake3: String,
    rewrite_paths: Vec<String>,
    old_link_stem: String,
    new_link_stem: String,
    phase: ConsolidationPhase,
}

#[derive(Debug)]
struct JournalEntry {
    vault_root: PathBuf,
    path: PathBuf,
    journal: ConsolidationJournal,
}

#[derive(Debug)]
struct ApplyProposalsOutcome {
    all_succeeded: bool,
    had_work: bool,
    summaries: Vec<String>,
    proposals: Vec<ConsolidationProposal>,
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
    if notes.is_empty() && !apply {
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

    if apply {
        match apply_proposals(store, &proposals, &notes) {
            Ok(outcome) => {
                warnings.extend(outcome.summaries);
                if outcome.all_succeeded && outcome.had_work {
                    warnings.push(
                        "Re-index the vault to update the graph: \
                         nestweaver brain refresh <vault-path>"
                            .to_string(),
                    );
                }
                return Ok(ConsolidationManifest {
                    dry_run: false,
                    applied: outcome.all_succeeded && outcome.had_work,
                    proposals: outcome.proposals,
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

/// Recover every incomplete vault-local journal, then execute newly discovered
/// proposals. New proposals are completely preflighted before the first
/// Prepared journal is written. Once a journal exists, its captured plan is
/// authoritative even when a re-run's graph no longer discovers the proposal.
fn apply_proposals(
    store: &GraphStore,
    proposals: &[ConsolidationProposal],
    notes: &[nestweaver_schema::Note],
) -> Result<ApplyProposalsOutcome> {
    let vaults = store.list_vaults(None).map_err(|e| anyhow::anyhow!(e))?;
    let vault_roots: HashMap<&str, PathBuf> = vaults
        .iter()
        .map(|v| {
            validate_vault_root(Path::new(v.root_path.as_str())).map(|root| (v.uid.as_str(), root))
        })
        .collect::<Result<_>>()?;
    let note_by_uid: HashMap<&str, &nestweaver_schema::Note> =
        notes.iter().map(|n| (n.uid.as_str(), n)).collect();

    // Loading and validating every existing journal is part of preflight. A
    // corrupt or foreign journal must stop application before any new note is
    // moved.
    let mut entries = load_consolidation_journals(&vaults, &vault_roots)?;
    // Prepare every fresh proposal in memory first. This catches ambiguous
    // same-source/same-destination batches and all current read/path failures
    // before publishing even the first Prepared checkpoint.
    let mut new_entries = Vec::new();
    for proposal in proposals {
        let note = note_by_uid
            .get(proposal.source_uid.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot prepare consolidation for missing note uid '{}'",
                    proposal.source_uid
                )
            })?;
        let vault_root = vault_roots.get(note.vault_uid.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot prepare consolidation for '{}': vault uid '{}' is unavailable",
                proposal.source_path,
                note.vault_uid
            )
        })?;
        let source_relative = validate_vault_relative_path(Path::new(&proposal.source_path))?;
        let destination_relative = destination_relative_path(proposal)?;
        let id = consolidation_journal_id(
            &note.vault_uid,
            &proposal.source_uid,
            &path_to_slash_string(&source_relative),
            &path_to_slash_string(&destination_relative),
        );
        let path = consolidation_journal_path(vault_root, &id);
        if let Some(existing) = entries
            .iter()
            .chain(new_entries.iter())
            .find(|entry| entry.path == path)
        {
            validate_journal_for_proposal(&existing.journal, proposal, note, &path)?;
            continue;
        }
        let journal = prepare_consolidation_journal(proposal, note, vault_root, &note_by_uid)?;
        new_entries.push(JournalEntry {
            vault_root: vault_root.clone(),
            path,
            journal,
        });
    }

    validate_non_conflicting_journals(entries.iter().chain(new_entries.iter()))?;

    // Only now does application begin. Each Prepared write is independently
    // durable, so even failure while preparing a later proposal leaves an
    // exact recovery point and no unjournaled note mutation.
    for entry in &new_entries {
        ensure_journal_directory(&entry.vault_root)?;
        write_consolidation_journal(&entry.vault_root, &entry.path, &entry.journal)?;
    }
    entries.extend(new_entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut summaries = Vec::new();
    let mut all_ok = true;
    let mut had_work = false;
    let mut result_proposals: Vec<_> = proposals
        .iter()
        .filter(|proposal| {
            !entries.iter().any(|entry| {
                entry.journal.phase == ConsolidationPhase::Complete
                    && entry.journal.proposal.source_uid == proposal.source_uid
                    && entry.journal.proposal.promote_to == proposal.promote_to
            })
        })
        .cloned()
        .collect();

    for entry in &mut entries {
        if entry.journal.phase == ConsolidationPhase::Complete {
            // A Complete journal is a transaction receipt, and it has exactly
            // one remaining job: keep a re-run idempotent while the graph still
            // names the source it consolidated. Two conditions end that job.
            //
            // DRIFT -- the vault no longer matches the receipt. The user
            // deleted or renamed the promoted note. Nothing can re-apply the
            // consolidation (source and destination are both gone), so the
            // receipt is obsolete, not broken. Reporting it as a failure made
            // one ordinary vault edit a PERMANENT failure on every future run,
            // clearable only by deleting journal files by hand.
            //
            // ABSORBED -- the vault has been re-indexed and the source uid is
            // gone from the graph, so no proposal can regenerate. Without this,
            // every consolidation a vault ever performs leaves a file that is
            // re-read and re-stat'd forever.
            let drift = validate_retained_complete_journal(&entry.vault_root, &entry.journal).err();
            let absorbed = !note_by_uid.contains_key(entry.journal.proposal.source_uid.as_str());
            if drift.is_some() || absorbed {
                match retire_consolidation_journal(&entry.path) {
                    // Only drift is worth telling the user about; an absorbed
                    // receipt retiring on schedule is not an event.
                    Ok(()) => {
                        if let Some(error) = drift {
                            summaries.push(format!(
                                "RETIRED: completed consolidation '{}' no longer matches the vault                                  and its receipt was removed from {}: {error}",
                                entry.journal.journal_id,
                                entry.path.display()
                            ));
                        }
                    }
                    // Failing to REMOVE the receipt is a real filesystem fault
                    // on this run, and is reported as one.
                    Err(error) => {
                        all_ok = false;
                        summaries.push(format!(
                            "FAILED: could not retire completed consolidation '{}' at {}: {error}",
                            entry.journal.journal_id,
                            entry.path.display()
                        ));
                    }
                }
            }
            continue;
        }
        had_work = true;
        if !result_proposals.iter().any(|proposal| {
            proposal.source_uid == entry.journal.proposal.source_uid
                && proposal.promote_to == entry.journal.proposal.promote_to
        }) {
            result_proposals.push(entry.journal.proposal.clone());
        }

        match apply_consolidation_journal(entry) {
            Ok(rewrite_count) => {
                let source = entry.vault_root.join(&entry.journal.source_path);
                let destination = entry.vault_root.join(&entry.journal.destination_path);
                summaries.push(format!(
                    "MOVED: {} → {}",
                    source.display(),
                    destination.display()
                ));
                if rewrite_count > 0 {
                    summaries.push(format!(
                        "  REWRITE: updated [[{}]] → [[{}]] in {rewrite_count} file(s)",
                        entry.journal.old_link_stem, entry.journal.new_link_stem
                    ));
                }
            }
            Err(error) => {
                all_ok = false;
                summaries.push(format!(
                    "FAILED: consolidation '{}' remains recoverable at {:?} in {}: {error}",
                    entry.journal.journal_id,
                    entry.journal.phase,
                    entry.path.display()
                ));
            }
        }
    }

    result_proposals.sort_by(|a, b| {
        a.source_uid
            .cmp(&b.source_uid)
            .then(a.promote_to.cmp(&b.promote_to))
    });
    result_proposals.dedup_by(|a, b| a.source_uid == b.source_uid && a.promote_to == b.promote_to);

    Ok(ApplyProposalsOutcome {
        all_succeeded: all_ok,
        had_work,
        summaries,
        proposals: result_proposals,
    })
}

fn prepare_consolidation_journal(
    proposal: &ConsolidationProposal,
    source_note: &nestweaver_schema::Note,
    vault_root: &Path,
    note_by_uid: &HashMap<&str, &nestweaver_schema::Note>,
) -> Result<ConsolidationJournal> {
    let source_relative = validate_vault_relative_path(Path::new(&proposal.source_path))?;
    if source_note.file_path != proposal.source_path {
        anyhow::bail!(
            "proposal source path '{}' does not match stored note path '{}'",
            proposal.source_path,
            source_note.file_path
        );
    }
    let destination_relative = destination_relative_path(proposal)?;
    if source_relative == destination_relative {
        anyhow::bail!(
            "consolidation source and destination are identical: {}",
            source_relative.display()
        );
    }

    let source = validated_vault_path(vault_root, &source_relative, "consolidation source")?;
    let destination = validated_vault_path(
        vault_root,
        &destination_relative,
        "consolidation destination",
    )?;
    let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
        anyhow::anyhow!("read consolidation source {}: {error}", source.display())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("consolidation source is not a file: {}", source.display());
    }
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => anyhow::bail!(
            "consolidation destination already exists without a journal: {}",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect consolidation destination {}: {error}",
                destination.display()
            ));
        }
    }
    let source_bytes = std::fs::read(&source).map_err(|error| {
        anyhow::anyhow!("read consolidation source {}: {error}", source.display())
    })?;

    let mut rewrite_paths = Vec::new();
    for uid in &proposal.evidence {
        let note = note_by_uid.get(uid.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot prepare wikilink rewrite: evidence note uid '{uid}' is unavailable"
            )
        })?;
        if note.vault_uid != source_note.vault_uid {
            anyhow::bail!(
                "cannot rewrite cross-vault evidence note '{}' for source '{}'",
                note.file_path,
                proposal.source_path
            );
        }
        let relative = validate_vault_relative_path(Path::new(&note.file_path))?;
        let path = validated_vault_path(vault_root, &relative, "wikilink rewrite target")?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            anyhow::anyhow!("preflight wikilink rewrite {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!(
                "preflight wikilink rewrite target is not a regular file: {}",
                path.display()
            );
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            anyhow::anyhow!("preflight wikilink rewrite {}: {error}", path.display())
        })?;
        std::str::from_utf8(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "preflight wikilink rewrite {} as UTF-8: {error}",
                path.display()
            )
        })?;
        rewrite_paths.push(path_to_slash_string(&relative));
    }
    rewrite_paths.sort();
    rewrite_paths.dedup();

    let source_path = path_to_slash_string(&source_relative);
    let destination_path = path_to_slash_string(&destination_relative);
    let old_link_stem = path_to_slash_string(&source_relative.with_extension(""));
    let new_link_stem = path_to_slash_string(&destination_relative.with_extension(""));
    let journal_id = consolidation_journal_id(
        &source_note.vault_uid,
        &proposal.source_uid,
        &source_path,
        &destination_path,
    );
    let mut captured_proposal = proposal.clone();
    captured_proposal.source_path = source_path.clone();

    Ok(ConsolidationJournal {
        version: CONSOLIDATION_JOURNAL_VERSION,
        journal_id,
        vault_uid: source_note.vault_uid.clone(),
        proposal: captured_proposal,
        source_path,
        destination_path,
        source_byte_len: source_bytes.len() as u64,
        source_blake3: crate::hash::blake3_hex_bytes(&source_bytes),
        rewrite_paths,
        old_link_stem,
        new_link_stem,
        phase: ConsolidationPhase::Prepared,
    })
}

fn destination_relative_path(proposal: &ConsolidationProposal) -> Result<PathBuf> {
    let source = validate_vault_relative_path(Path::new(&proposal.source_path))?;
    let file_name = source.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot extract a filename from consolidation source '{}'",
            proposal.source_path
        )
    })?;
    let parent = if proposal.promote_to == "_ideas" {
        PathBuf::from("_ideas")
    } else if let Some(project) = proposal
        .promote_to
        .strip_prefix("project-file (")
        .and_then(|value| value.strip_suffix(')'))
    {
        validate_vault_relative_path(Path::new(project))?
    } else {
        anyhow::bail!("unknown consolidation target '{}'", proposal.promote_to);
    };
    validate_vault_relative_path(&parent.join(file_name))
}

fn validate_vault_relative_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        anyhow::bail!(
            "vault path must be non-empty and relative: {}",
            path.display()
        );
    }
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        anyhow::bail!(
            "vault path contains a non-normal component: {}",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

fn validate_vault_root(vault_root: &Path) -> Result<PathBuf> {
    let absolute = if vault_root.is_absolute() {
        vault_root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolve current directory: {error}"))?
            .join(vault_root)
    };
    // `symlink_metadata("alias/")` follows `alias` on POSIX because the
    // trailing separator requires directory traversal. Rebuild the lexical
    // spelling from components first so the final component is always checked
    // without a trailing separator or terminal `/.` bypass.
    let lexical_root: PathBuf = absolute.components().collect();
    let metadata = std::fs::symlink_metadata(&lexical_root).map_err(|error| {
        anyhow::anyhow!("inspect vault root {}: {error}", lexical_root.display())
    })?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("vault root is a symlink: {}", lexical_root.display());
    }
    if !metadata.is_dir() {
        anyhow::bail!("vault root is not a directory: {}", lexical_root.display());
    }
    std::fs::canonicalize(&lexical_root).map_err(|error| {
        anyhow::anyhow!(
            "canonicalize vault root {}: {error}",
            lexical_root.display()
        )
    })
}

/// Resolve one validated vault-relative path after rejecting symlinks and
/// non-directories in every existing parent component. The leaf may be absent,
/// but an existing leaf is never allowed to be a symlink.
fn validated_vault_path(vault_root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    let relative = validate_vault_relative_path(relative)?;
    let vault_root = validate_vault_root(vault_root)?;
    let mut current = vault_root.clone();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                unreachable!("validated path contains only normal components");
            };
            current.push(name);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    anyhow::bail!("{label} parent is a symlink: {}", current.display())
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => anyhow::bail!("{label} parent is not a directory: {}", current.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "inspect {label} parent {}: {error}",
                        current.display()
                    ));
                }
            }
        }
    }
    let path = vault_root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("{label} is a symlink: {}", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect {label} {}: {error}",
                path.display()
            ));
        }
    }
    Ok(path)
}

fn path_to_slash_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn consolidation_journal_id(
    vault_uid: &str,
    source_uid: &str,
    source_path: &str,
    destination_path: &str,
) -> String {
    crate::hash::blake3_hex(&format!(
        "v1\0{vault_uid}\0{source_uid}\0{source_path}\0{destination_path}"
    ))
}

fn consolidation_journal_path(vault_root: &Path, journal_id: &str) -> PathBuf {
    vault_root
        .join(CONSOLIDATION_JOURNAL_DIR)
        .join(format!("{journal_id}.json"))
}

fn ensure_journal_directory(vault_root: &Path) -> Result<()> {
    ensure_vault_directory(vault_root, Path::new(CONSOLIDATION_JOURNAL_DIR))
}

fn ensure_vault_directory(vault_root: &Path, relative: &Path) -> Result<()> {
    let relative = validate_vault_relative_path(relative)?;
    let mut current = validate_vault_root(vault_root)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            unreachable!("validated path contains only normal components");
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                "required consolidation directory is a symlink: {}",
                current.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => anyhow::bail!(
                "required consolidation directory is not a directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|error| {
                    anyhow::anyhow!(
                        "create consolidation directory {}: {error}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "inspect consolidation directory {}: {error}",
                    current.display()
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            anyhow::anyhow!(
                "re-inspect consolidation directory {}: {error}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!(
                "required consolidation directory changed into a symlink or non-directory: {}",
                current.display()
            );
        }
        // Sync even when the directory was already visible: a prior attempt
        // may have created it and then failed the parent sync.
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&current).map_err(
            |error| {
                anyhow::anyhow!(
                    "publish consolidation directory {} durably: {error}",
                    current.display()
                )
            },
        )?;
    }
    Ok(())
}

/// Remove a receipt that has no remaining purpose. Durable, so a crash cannot
/// leave it half-unlinked and reappearing on the next run.
fn retire_consolidation_journal(path: &Path) -> Result<()> {
    match nestweaver_store::durable_sidecar::remove_file_durable(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "retire consolidation journal {}: {error}",
            path.display()
        )),
    }
}

fn write_consolidation_journal(
    vault_root: &Path,
    path: &Path,
    journal: &ConsolidationJournal,
) -> Result<()> {
    let relative = path.strip_prefix(vault_root).map_err(|_| {
        anyhow::anyhow!(
            "consolidation journal path escapes vault root: {}",
            path.display()
        )
    })?;
    let path = validated_vault_path(vault_root, relative, "consolidation journal")?;
    let mut bytes = serde_json::to_vec_pretty(journal)?;
    bytes.push(b'\n');
    nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| file.write_all(&bytes))
        .map_err(|error| {
            anyhow::anyhow!(
                "write consolidation journal {} durably: {error}",
                path.display()
            )
        })
}

fn load_consolidation_journals(
    vaults: &[nestweaver_schema::Vault],
    vault_roots: &HashMap<&str, PathBuf>,
) -> Result<Vec<JournalEntry>> {
    let mut entries = Vec::new();
    for vault in vaults {
        let vault_root = vault_roots.get(vault.uid.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot load consolidation journals: vault uid '{}' has no snapshotted root",
                vault.uid
            )
        })?;
        let directory = validated_vault_path(
            vault_root,
            Path::new(CONSOLIDATION_JOURNAL_DIR),
            "consolidation journal directory",
        )?;
        let read_dir = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                "consolidation journal directory is a symlink: {}",
                directory.display()
            ),
            Ok(metadata) if !metadata.is_dir() => anyhow::bail!(
                "consolidation journal path is not a directory: {}",
                directory.display()
            ),
            Ok(_) => std::fs::read_dir(&directory).map_err(|error| {
                anyhow::anyhow!(
                    "read consolidation journal directory {}: {error}",
                    directory.display()
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "inspect consolidation journal directory {}: {error}",
                    directory.display()
                ));
            }
        };
        let mut paths = Vec::new();
        for item in read_dir {
            let path = item
                .map_err(|error| {
                    anyhow::anyhow!(
                        "enumerate consolidation journal directory {}: {error}",
                        directory.display()
                    )
                })?
                .path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            let relative = path.strip_prefix(vault_root).map_err(|_| {
                anyhow::anyhow!(
                    "consolidation journal path escapes vault root: {}",
                    path.display()
                )
            })?;
            let path = validated_vault_path(vault_root, relative, "consolidation journal")?;
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                anyhow::anyhow!("inspect consolidation journal {}: {error}", path.display())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!(
                    "consolidation journal is not a regular file: {}",
                    path.display()
                );
            }
            // A prior atomic replacement may have returned after the rename but
            // before its directory sync. Confirm that namespace change before
            // trusting the checkpoint.
            nestweaver_store::durable_sidecar::sync_parent_directory_durable(&path).map_err(
                |error| {
                    anyhow::anyhow!(
                        "confirm consolidation journal {} durably: {error}",
                        path.display()
                    )
                },
            )?;
            let bytes = std::fs::read(&path).map_err(|error| {
                anyhow::anyhow!("read consolidation journal {}: {error}", path.display())
            })?;
            let journal: ConsolidationJournal =
                serde_json::from_slice(&bytes).map_err(|error| {
                    anyhow::anyhow!("parse consolidation journal {}: {error}", path.display())
                })?;
            validate_loaded_journal(&journal, &vault.uid, vault_root, &path)?;
            entries.push(JournalEntry {
                vault_root: vault_root.clone(),
                path,
                journal,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn validate_loaded_journal(
    journal: &ConsolidationJournal,
    vault_uid: &str,
    vault_root: &Path,
    path: &Path,
) -> Result<()> {
    if journal.version != CONSOLIDATION_JOURNAL_VERSION {
        anyhow::bail!(
            "unsupported consolidation journal version {} in {}",
            journal.version,
            path.display()
        );
    }
    if journal.vault_uid != vault_uid {
        anyhow::bail!(
            "consolidation journal {} belongs to vault '{}' rather than '{}'",
            path.display(),
            journal.vault_uid,
            vault_uid
        );
    }
    let source = validate_vault_relative_path(Path::new(&journal.source_path))?;
    let destination = validate_vault_relative_path(Path::new(&journal.destination_path))?;
    if path_to_slash_string(&source) != journal.source_path
        || path_to_slash_string(&destination) != journal.destination_path
    {
        anyhow::bail!(
            "non-canonical path in consolidation journal {}",
            path.display()
        );
    }
    if journal.proposal.source_path != journal.source_path {
        anyhow::bail!(
            "proposal/source mismatch in consolidation journal {}",
            path.display()
        );
    }
    let derived_destination = destination_relative_path(&journal.proposal)?;
    if derived_destination != destination {
        anyhow::bail!(
            "proposal/destination mismatch in consolidation journal {}",
            path.display()
        );
    }
    if journal.old_link_stem != path_to_slash_string(&source.with_extension(""))
        || journal.new_link_stem != path_to_slash_string(&destination.with_extension(""))
    {
        anyhow::bail!(
            "wikilink stem mismatch in consolidation journal {}",
            path.display()
        );
    }
    let mut normalized_rewrites = Vec::with_capacity(journal.rewrite_paths.len());
    for rewrite in &journal.rewrite_paths {
        let normalized = validate_vault_relative_path(Path::new(rewrite))?;
        normalized_rewrites.push(path_to_slash_string(&normalized));
    }
    let mut sorted_rewrites = normalized_rewrites.clone();
    sorted_rewrites.sort();
    sorted_rewrites.dedup();
    if normalized_rewrites != sorted_rewrites {
        anyhow::bail!(
            "rewrite paths are not canonical, sorted, and unique in consolidation journal {}",
            path.display()
        );
    }
    if journal.source_blake3.len() != 64
        || !journal
            .source_blake3
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!(
            "invalid source digest in consolidation journal {}",
            path.display()
        );
    }
    let expected_id = consolidation_journal_id(
        vault_uid,
        &journal.proposal.source_uid,
        &journal.source_path,
        &journal.destination_path,
    );
    if journal.journal_id != expected_id
        || path != consolidation_journal_path(vault_root, &expected_id)
    {
        anyhow::bail!(
            "identity mismatch in consolidation journal {}",
            path.display()
        );
    }
    validate_journal_vault_paths(vault_root, journal, path)?;
    Ok(())
}

fn validate_journal_vault_paths(
    vault_root: &Path,
    journal: &ConsolidationJournal,
    journal_path: &Path,
) -> Result<()> {
    validated_vault_path(
        vault_root,
        Path::new(&journal.source_path),
        "consolidation source",
    )?;
    validated_vault_path(
        vault_root,
        Path::new(&journal.destination_path),
        "consolidation destination",
    )?;
    for rewrite in &journal.rewrite_paths {
        validated_vault_path(vault_root, Path::new(rewrite), "wikilink rewrite target")?;
    }
    let journal_relative = journal_path.strip_prefix(vault_root).map_err(|_| {
        anyhow::anyhow!(
            "consolidation journal path escapes vault root: {}",
            journal_path.display()
        )
    })?;
    validated_vault_path(vault_root, journal_relative, "consolidation journal")?;
    Ok(())
}

fn validate_journal_for_proposal(
    existing: &ConsolidationJournal,
    proposal: &ConsolidationProposal,
    note: &nestweaver_schema::Note,
    path: &Path,
) -> Result<()> {
    let proposal_source = path_to_slash_string(&validate_vault_relative_path(Path::new(
        &proposal.source_path,
    ))?);
    if existing.vault_uid != note.vault_uid
        || existing.proposal.source_uid != proposal.source_uid
        || existing.source_path != proposal_source
        || existing.proposal.promote_to != proposal.promote_to
    {
        anyhow::bail!(
            "fresh proposal conflicts with durable consolidation journal {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_non_conflicting_journals<'a>(
    entries: impl Iterator<Item = &'a JournalEntry>,
) -> Result<()> {
    let active: Vec<&JournalEntry> = entries
        .filter(|entry| entry.journal.phase != ConsolidationPhase::Complete)
        .collect();
    let mut sources: HashMap<(PathBuf, String), String> = HashMap::new();
    let mut destinations: HashMap<(PathBuf, String), String> = HashMap::new();
    for entry in &active {
        let source_key = (entry.vault_root.clone(), entry.journal.source_path.clone());
        if let Some(other) = sources.insert(source_key, entry.journal.journal_id.clone())
            && other != entry.journal.journal_id
        {
            anyhow::bail!(
                "conflicting consolidation journals '{other}' and '{}' share source '{}'",
                entry.journal.journal_id,
                entry.journal.source_path
            );
        }
        let destination_key = (
            entry.vault_root.clone(),
            entry.journal.destination_path.clone(),
        );
        if let Some(other) = destinations.insert(destination_key, entry.journal.journal_id.clone())
            && other != entry.journal.journal_id
        {
            anyhow::bail!(
                "conflicting consolidation journals '{other}' and '{}' share destination '{}'",
                entry.journal.journal_id,
                entry.journal.destination_path
            );
        }
    }
    for entry in &active {
        for rewrite in &entry.journal.rewrite_paths {
            if let Some(source_journal) = sources.get(&(entry.vault_root.clone(), rewrite.clone()))
            {
                anyhow::bail!(
                    "consolidation journal '{}' would rewrite source '{}' owned by journal '{}'",
                    entry.journal.journal_id,
                    rewrite,
                    source_journal
                );
            }
        }
    }
    Ok(())
}

fn apply_consolidation_journal(entry: &mut JournalEntry) -> Result<usize> {
    let mut rewrite_count = 0;
    loop {
        validate_journal_vault_paths(&entry.vault_root, &entry.journal, &entry.path)?;
        match entry.journal.phase {
            ConsolidationPhase::Prepared => {
                publish_consolidation_destination(&entry.vault_root, &entry.journal)?;
                advance_consolidation_phase(entry, ConsolidationPhase::DestinationPublished)?;
            }
            ConsolidationPhase::DestinationPublished => {
                remove_consolidation_source(&entry.vault_root, &entry.journal)?;
                advance_consolidation_phase(entry, ConsolidationPhase::SourceRemoved)?;
            }
            ConsolidationPhase::SourceRemoved => {
                rewrite_count += rewrite_journal_wikilinks(&entry.vault_root, &entry.journal)?;
                advance_consolidation_phase(entry, ConsolidationPhase::RewritesApplied)?;
            }
            ConsolidationPhase::RewritesApplied => {
                // Validate the exact captured bytes and rewrites before the
                // durable Complete checkpoint. Once Complete is published,
                // later user edits to the destination are legitimate and the
                // retained journal must not freeze them forever.
                validate_consolidation_completion(&entry.vault_root, &entry.journal)?;
                advance_consolidation_phase(entry, ConsolidationPhase::Complete)?;
            }
            ConsolidationPhase::Complete => {
                validate_retained_complete_journal(&entry.vault_root, &entry.journal)?;
                return Ok(rewrite_count);
            }
        }
    }
}

fn advance_consolidation_phase(entry: &mut JournalEntry, phase: ConsolidationPhase) -> Result<()> {
    let mut next = entry.journal.clone();
    next.phase = phase;
    write_consolidation_journal(&entry.vault_root, &entry.path, &next)?;
    entry.journal = next;
    Ok(())
}

fn publish_consolidation_destination(
    vault_root: &Path,
    journal: &ConsolidationJournal,
) -> Result<()> {
    let source_relative = Path::new(&journal.source_path);
    let source = validated_vault_path(vault_root, source_relative, "consolidation source")?;
    let destination_relative = Path::new(&journal.destination_path);
    let destination = validated_vault_path(
        vault_root,
        destination_relative,
        "consolidation destination",
    )?;
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            anyhow::bail!(
                "published consolidation destination is not a regular file: {}",
                destination.display()
            );
        }
        Ok(_) => {
            validate_journal_file(&destination, journal)?;
            nestweaver_store::durable_sidecar::sync_parent_directory_durable(&destination)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "confirm published consolidation destination {}: {error}",
                        destination.display()
                    )
                })?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect consolidation destination {}: {error}",
                destination.display()
            ));
        }
    }

    validate_journal_file(&source, journal)?;
    let parent_relative = destination_relative.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "consolidation destination has no parent: {}",
            destination.display()
        )
    })?;
    ensure_vault_directory(vault_root, parent_relative)?;
    let source = validated_vault_path(vault_root, source_relative, "consolidation source")?;
    let destination = validated_vault_path(
        vault_root,
        destination_relative,
        "consolidation destination",
    )?;
    let mut input = std::fs::File::open(&source).map_err(|error| {
        anyhow::anyhow!("open consolidation source {}: {error}", source.display())
    })?;
    nestweaver_store::durable_sidecar::atomic_replace_file(&destination, |output| {
        std::io::copy(&mut input, output).map(|_| ())
    })
    .map_err(|error| {
        anyhow::anyhow!(
            "publish consolidation destination {}: {error}",
            destination.display()
        )
    })?;
    validate_journal_file(&destination, journal)
}

fn validate_journal_file(path: &Path, journal: &ConsolidationJournal) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        anyhow::anyhow!("inspect consolidation file {}: {error}", path.display())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "consolidation file is not a regular file: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("read consolidation file {}: {error}", path.display()))?;
    let digest = crate::hash::blake3_hex_bytes(&bytes);
    if bytes.len() as u64 != journal.source_byte_len || digest != journal.source_blake3 {
        anyhow::bail!(
            "consolidation file {} no longer matches journaled source bytes",
            path.display()
        );
    }
    Ok(())
}

fn remove_consolidation_source(vault_root: &Path, journal: &ConsolidationJournal) -> Result<()> {
    let source = validated_vault_path(
        vault_root,
        Path::new(&journal.source_path),
        "consolidation source",
    )?;
    match std::fs::symlink_metadata(&source) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!(
                    "refuse to unlink non-file consolidation source {}",
                    source.display()
                );
            }
            validate_journal_file(&source, journal)?;
            nestweaver_store::durable_sidecar::remove_file_durable(&source).map_err(|error| {
                anyhow::anyhow!("remove consolidation source {}: {error}", source.display())
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The unlink may have happened before a prior attempt failed its
            // parent sync. Syncing now turns observed absence into durable
            // proof before the phase advances.
            nestweaver_store::durable_sidecar::sync_parent_directory_durable(&source).map_err(
                |error| {
                    anyhow::anyhow!(
                        "confirm removed consolidation source {}: {error}",
                        source.display()
                    )
                },
            )?;
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect consolidation source {}: {error}",
                source.display()
            ));
        }
    }
    Ok(())
}

fn rewrite_journal_wikilinks(vault_root: &Path, journal: &ConsolidationJournal) -> Result<usize> {
    if journal.old_link_stem == journal.new_link_stem {
        return Ok(0);
    }
    let old_link = format!("[[{}]]", journal.old_link_stem);
    let new_link = format!("[[{}]]", journal.new_link_stem);
    let old_link_prefix = format!("[[{}|", journal.old_link_stem);
    let new_link_prefix = format!("[[{}|", journal.new_link_stem);
    let mut count = 0;

    for relative in &journal.rewrite_paths {
        let relative = validate_vault_relative_path(Path::new(relative))?;
        let path = validated_vault_path(vault_root, &relative, "wikilink rewrite target")?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            anyhow::anyhow!(
                "inspect wikilink rewrite target {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!(
                "wikilink rewrite target is not a regular file: {}",
                path.display()
            );
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            anyhow::anyhow!("read wikilink rewrite target {}: {error}", path.display())
        })?;
        let content = std::str::from_utf8(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "read wikilink rewrite target {} as UTF-8: {error}",
                path.display()
            )
        })?;
        let updated = content
            .replace(&old_link, &new_link)
            .replace(&old_link_prefix, &new_link_prefix);
        if updated == content {
            // This can be an idempotent retry after rename succeeded but its
            // parent sync failed. Re-sync before treating the rewrite as done.
            nestweaver_store::durable_sidecar::sync_parent_directory_durable(&path).map_err(
                |error| {
                    anyhow::anyhow!(
                        "confirm unchanged/idempotent wikilink target {}: {error}",
                        path.display()
                    )
                },
            )?;
            continue;
        }
        let path = validated_vault_path(vault_root, &relative, "wikilink rewrite target")?;
        nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
            file.write_all(updated.as_bytes())
        })
        .map_err(|error| {
            anyhow::anyhow!("rewrite wikilinks in {} durably: {error}", path.display())
        })?;
        count += 1;
    }
    Ok(count)
}

fn validate_consolidation_completion(
    vault_root: &Path,
    journal: &ConsolidationJournal,
) -> Result<()> {
    let source = validated_vault_path(
        vault_root,
        Path::new(&journal.source_path),
        "consolidation source",
    )?;
    match std::fs::symlink_metadata(&source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!(
            "completed consolidation source unexpectedly exists: {}",
            source.display()
        ),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect completed consolidation source {}: {error}",
                source.display()
            ));
        }
    }
    let destination = validated_vault_path(
        vault_root,
        Path::new(&journal.destination_path),
        "consolidation destination",
    )?;
    validate_journal_file(&destination, journal)?;

    let old_link = format!("[[{}]]", journal.old_link_stem);
    let old_link_prefix = format!("[[{}|", journal.old_link_stem);
    for relative in &journal.rewrite_paths {
        let path = validated_vault_path(
            vault_root,
            &validate_vault_relative_path(Path::new(relative))?,
            "wikilink rewrite target",
        )?;
        let content = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!(
                "validate completed wikilink target {}: {error}",
                path.display()
            )
        })?;
        if content.contains(&old_link) || content.contains(&old_link_prefix) {
            anyhow::bail!(
                "completed consolidation still has an old wikilink in {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_retained_complete_journal(
    vault_root: &Path,
    journal: &ConsolidationJournal,
) -> Result<()> {
    let source = validated_vault_path(
        vault_root,
        Path::new(&journal.source_path),
        "consolidation source",
    )?;
    match std::fs::symlink_metadata(&source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!(
            "completed consolidation source unexpectedly exists: {}",
            source.display()
        ),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "inspect completed consolidation source {}: {error}",
                source.display()
            ));
        }
    }

    let destination = validated_vault_path(
        vault_root,
        Path::new(&journal.destination_path),
        "consolidation destination",
    )?;
    let metadata = std::fs::symlink_metadata(&destination).map_err(|error| {
        anyhow::anyhow!(
            "inspect completed consolidation destination {}: {error}",
            destination.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "completed consolidation destination is not a file: {}",
            destination.display()
        );
    }
    Ok(())
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
    fn lint_does_not_union_unrelated_templates_into_one_kind() {
        // nw-307 / F-HEALTH-2: `NoteKind::from_hint` collapses every
        // unrecognised template stem to `General`, and `load_templates` then
        // UNIONS their key sets into the single "general" bucket. A daily log
        // that matches `_templates/Log.md` EXACTLY is flagged for missing
        // Backlog-Item keys it was never supposed to have.
        let (_dir, root) = make_vault(&[
            (
                "_templates/Log.md",
                "---\nPeople:\ntags: [type/daily-log]\n---\n# Log Template\n",
            ),
            (
                "_templates/Backlog Item.md",
                "---\nid:\npriority:\nstatus:\npromoted:\n---\n# Backlog Item Template\n",
            ),
            (
                "_logs/2024-03-26.md",
                "---\ntype: log\nPeople:\ntags: [type/daily-log]\n---\n# 2024-03-26\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();

        let drift = report
            .schema_drift
            .iter()
            .find(|d| d.file_path.ends_with("2024-03-26.md"));
        assert!(
            drift.is_none(),
            "a note matching its own template exactly must not drift; \
             it was flagged for {:?} — keys belonging to Backlog Item",
            drift.map(|d| &d.missing_keys)
        );
    }

    /// nw-307, the other half of the same bucket bug: the templates themselves
    /// are notes, so they lint against the merged bucket and every template is
    /// reported as drifting from every other template.
    #[test]
    fn lint_does_not_flag_the_template_notes_themselves() {
        let (_dir, root) = make_vault(&[
            (
                "_templates/Log.md",
                "---\nPeople:\ntags: [type/daily-log]\n---\n# Log Template\n",
            ),
            (
                "_templates/Backlog Item.md",
                "---\nid:\npriority:\nstatus:\npromoted:\n---\n# Backlog Item Template\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        let flagged: Vec<&str> = report
            .schema_drift
            .iter()
            .filter(|d| d.file_path.to_lowercase().contains("_templates/"))
            .map(|d| d.file_path.as_str())
            .collect();
        assert!(
            flagged.is_empty(),
            "a template defines the schema; it cannot drift from it — got {flagged:?}"
        );
    }

    /// nw-307's guard against over-correcting: a note whose declared kind DOES
    /// have a template must still be checked against exactly that template.
    #[test]
    fn lint_still_flags_drift_against_the_notes_own_template() {
        let (_dir, root) = make_vault(&[
            (
                "_templates/Log.md",
                "---\nPeople:\ntags: [type/daily-log]\n---\n# Log Template\n",
            ),
            (
                "_templates/Backlog Item.md",
                "---\nid:\npriority:\nstatus:\npromoted:\n---\n# Backlog Item Template\n",
            ),
            // Declares `type: backlog item` but carries only `id`.
            (
                "items/one.md",
                "---\ntype: backlog item\nid: nw-1\n---\n# One\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let report = memory_lint(&store, 1_900_000_000.0).unwrap();
        let drift = report
            .schema_drift
            .iter()
            .find(|d| d.file_path.ends_with("items/one.md"))
            .expect("a backlog item missing priority/status/promoted must drift");
        let mut missing = drift.missing_keys.clone();
        missing.sort();
        assert_eq!(
            missing,
            vec![
                "priority".to_string(),
                "promoted".to_string(),
                "status".to_string()
            ],
            "only its OWN template's keys — never the Log template's"
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

    /// A retained `Complete` journal is a transaction receipt. The sibling
    /// test above pins that EDITING the destination does not fail; DELETING or
    /// renaming it must not either. Nothing can re-apply the consolidation --
    /// both source and destination are gone -- so the receipt is merely
    /// obsolete. Reporting it as `FAILED` on every future run makes an
    /// ordinary vault edit a PERMANENT failure with no way to clear it short
    /// of deleting journal files by hand.
    #[test]
    fn a_deleted_destination_retires_its_receipt_instead_of_failing_forever() {
        let (_dir, root, store, journal) = journal_fixture();
        let journal_path = persist_test_journal(&root, &journal);
        assert!(memory_consolidate(&store, true, 0.0).unwrap().applied);
        assert_eq!(
            read_test_journal(&journal_path).phase,
            ConsolidationPhase::Complete
        );

        // Ordinary vault hygiene: the user deletes the promoted note.
        fs::remove_file(root.join(&journal.destination_path)).unwrap();

        let after_delete = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();
        assert!(
            after_delete
                .warnings
                .iter()
                .all(|warning| !warning.contains("FAILED")),
            "a deleted destination is drift, not a failure: {:?}",
            after_delete.warnings
        );
        assert!(
            !journal_path.exists(),
            "an obsolete receipt must be retired, not retained to fail again"
        );

        // The property that matters: it does not recur.
        let later = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();
        assert!(
            later
                .warnings
                .iter()
                .all(|warning| !warning.contains("FAILED")),
            "the failure must not be permanent: {:?}",
            later.warnings
        );
    }

    /// Journals are crash-recovery records, and their one live use after
    /// `Complete` is keeping a re-run idempotent while the graph still names
    /// the consolidated source. Once the vault has been re-indexed and that
    /// source note is gone from the graph, no proposal can regenerate and the
    /// receipt has no remaining purpose -- retire it, so the directory does not
    /// grow without bound on every consolidation the vault ever performs.
    #[test]
    fn a_completed_journal_is_retired_once_the_graph_absorbs_it() {
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
        assert!(
            memory_consolidate(&store, true, 4_000_000_000.0)
                .unwrap()
                .applied
        );

        let journal_dir = root.join(CONSOLIDATION_JOURNAL_DIR);
        assert_eq!(
            fs::read_dir(&journal_dir).unwrap().count(),
            1,
            "one completed journal is retained while the graph is still stale"
        );

        // Re-index: the graph now reflects the moved note, so the source uid
        // the receipt names no longer exists and no proposal can regenerate.
        let (_res, reindexed) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        memory_consolidate(&reindexed, true, 4_000_000_000.0).unwrap();

        assert_eq!(
            fs::read_dir(&journal_dir).unwrap().count(),
            0,
            "an absorbed receipt must be retired, not retained forever"
        );
    }

    fn prepared_log_promotion_journal(store: &GraphStore, root: &Path) -> ConsolidationJournal {
        let manifest = memory_consolidate(store, false, 4_000_000_000.0).unwrap();
        let proposal = manifest
            .proposals
            .into_iter()
            .find(|proposal| proposal.source_path == "_logs/2025-01-01.md")
            .expect("log promotion proposal");
        let notes = store.list_notes(None).unwrap();
        let source_note = notes
            .iter()
            .find(|note| note.uid == proposal.source_uid)
            .unwrap();
        let note_by_uid: HashMap<&str, &nestweaver_schema::Note> =
            notes.iter().map(|note| (note.uid.as_str(), note)).collect();
        prepare_consolidation_journal(&proposal, source_note, root, &note_by_uid).unwrap()
    }

    fn journal_fixture() -> (tempfile::TempDir, PathBuf, GraphStore, ConsolidationJournal) {
        let (dir, root) = make_vault(&[
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
        let journal = prepared_log_promotion_journal(&store, &root);
        (dir, root, store, journal)
    }

    fn persist_test_journal(root: &Path, journal: &ConsolidationJournal) -> PathBuf {
        ensure_journal_directory(root).unwrap();
        let path = consolidation_journal_path(root, &journal.journal_id);
        write_consolidation_journal(root, &path, journal).unwrap();
        path
    }

    fn read_test_journal(path: &Path) -> ConsolidationJournal {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn vault_root_canonicalizes_platform_ancestor_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real-parent");
        let real_vault = real_parent.join("vault");
        fs::create_dir_all(&real_vault).unwrap();
        fs::write(real_vault.join("note.md"), "# Note\n").unwrap();
        let alias_parent = dir.path().join("platform-alias");
        symlink(&real_parent, &alias_parent).unwrap();

        let resolved = validated_vault_path(
            &alias_parent.join("vault"),
            Path::new("note.md"),
            "fixture note",
        )
        .unwrap();

        assert_eq!(
            resolved,
            fs::canonicalize(real_vault.join("note.md")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn vault_root_rejects_a_final_symlink_even_with_a_trailing_separator() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_vault = dir.path().join("real-vault");
        fs::create_dir(&real_vault).unwrap();
        let alias = dir.path().join("vault-alias");
        symlink(&real_vault, &alias).unwrap();
        let trailing = PathBuf::from(format!("{}/", alias.display()));

        let error = validate_vault_root(&trailing).unwrap_err();
        assert!(error.to_string().contains("vault root is a symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn journal_recovery_uses_one_snapshotted_physical_vault_root() {
        use std::os::unix::fs::symlink;

        let (dir, root, store, journal) = journal_fixture();
        let journal_path = persist_test_journal(&root, &journal);
        let root_name = root.file_name().unwrap();
        let alias_parent = dir.path().join("platform-alias");
        symlink(dir.path(), &alias_parent).unwrap();
        let configured_root = alias_parent.join(root_name);

        let mut vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        vaults[0].root_path = configured_root.display().to_string();
        let vault_roots: HashMap<&str, PathBuf> = vaults
            .iter()
            .map(|vault| {
                (
                    vault.uid.as_str(),
                    validate_vault_root(Path::new(&vault.root_path)).unwrap(),
                )
            })
            .collect();

        fs::remove_file(&alias_parent).unwrap();
        let other_parent = dir.path().join("other-parent");
        fs::create_dir(&other_parent).unwrap();
        fs::create_dir(other_parent.join(root_name)).unwrap();
        symlink(&other_parent, &alias_parent).unwrap();

        let entries = load_consolidation_journals(&vaults, &vault_roots).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, fs::canonicalize(&journal_path).unwrap());
        assert_eq!(entries[0].vault_root, fs::canonicalize(&root).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn consolidate_rejects_symlinked_destination_parent_before_any_mutation() {
        use std::os::unix::fs::symlink;

        let (dir, root, store, journal) = journal_fixture();
        let source = root.join(&journal.source_path);
        let source_before = fs::read(&source).unwrap();
        fs::rename(root.join("_ideas"), root.join("idea-evidence")).unwrap();
        let outside = dir.path().join("outside-destination");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel.txt"), "outside must stay unchanged").unwrap();
        symlink(&outside, root.join("_ideas")).unwrap();

        let manifest = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();

        assert!(!manifest.applied);
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("symlink")),
            "warnings: {:?}",
            manifest.warnings
        );
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(
            fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
            "outside must stay unchanged"
        );
        assert!(!outside.join("2025-01-01.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn consolidate_rejects_symlinked_rewrite_parent_before_any_mutation() {
        use std::os::unix::fs::symlink;

        let (dir, root, store, mut journal) = journal_fixture();
        let source = root.join(&journal.source_path);
        let source_before = fs::read(&source).unwrap();
        let rewrite_parent = root.join("rewrites");
        fs::create_dir(&rewrite_parent).unwrap();
        fs::write(
            rewrite_parent.join("link.md"),
            "[[_logs/2025-01-01]] inside vault",
        )
        .unwrap();
        journal.rewrite_paths = vec!["rewrites/link.md".to_string()];
        let _journal_path = persist_test_journal(&root, &journal);
        fs::remove_dir_all(&rewrite_parent).unwrap();
        let outside = dir.path().join("outside-rewrites");
        fs::create_dir(&outside).unwrap();
        let outside_rewrite = outside.join("link.md");
        let outside_before = "[[_logs/2025-01-01]] outside vault";
        fs::write(&outside_rewrite, outside_before).unwrap();
        symlink(&outside, &rewrite_parent).unwrap();

        let manifest = memory_consolidate(&store, true, 0.0).unwrap();

        assert!(!manifest.applied);
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("symlink")),
            "warnings: {:?}",
            manifest.warnings
        );
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert!(!root.join(&journal.destination_path).exists());
        assert_eq!(fs::read_to_string(outside_rewrite).unwrap(), outside_before);
    }

    #[cfg(unix)]
    #[test]
    fn consolidate_rejects_symlinked_journal_directory_before_any_mutation() {
        use std::os::unix::fs::symlink;

        let (dir, root, store, journal) = journal_fixture();
        let source = root.join(&journal.source_path);
        let source_before = fs::read(&source).unwrap();
        let outside = dir.path().join("outside-journals");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel.txt");
        fs::write(&sentinel, "not a journal").unwrap();
        symlink(&outside, root.join(CONSOLIDATION_JOURNAL_DIR)).unwrap();

        let manifest = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();

        assert!(!manifest.applied);
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("symlink")),
            "warnings: {:?}",
            manifest.warnings
        );
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert!(!root.join(&journal.destination_path).exists());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "not a journal");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn consolidate_recovers_existing_journal_when_discovery_is_empty() {
        let (_dir, root, store, journal) = journal_fixture();
        let journal_path = persist_test_journal(&root, &journal);
        // Simulate interruption after the atomic destination rename but before
        // Prepared could be advanced to DestinationPublished.
        publish_consolidation_destination(&root, &journal).unwrap();

        // `now=0` makes the indexed log too young, so fresh discovery is empty.
        // Recovery must still use the captured journal plan.
        let manifest = memory_consolidate(&store, true, 0.0).unwrap();
        assert!(manifest.applied, "warnings: {:?}", manifest.warnings);
        assert_eq!(manifest.proposals.len(), 1, "recovered work stays visible");
        assert!(!root.join("_logs/2025-01-01.md").exists());
        assert!(root.join("_ideas/2025-01-01.md").exists());
        assert_eq!(
            read_test_journal(&journal_path).phase,
            ConsolidationPhase::Complete
        );

        // A retained Complete journal is not new work once discovery is empty.
        let second = memory_consolidate(&store, true, 0.0).unwrap();
        assert!(!second.applied);
        assert!(second.proposals.is_empty());

        // A retained Complete journal is a transaction receipt, not a
        // permanent content lock. The stale graph can rediscover the removed
        // source, and users may legitimately edit the promoted destination.
        fs::write(
            root.join(&journal.destination_path),
            "# Promoted and subsequently edited\n",
        )
        .unwrap();
        let stale_graph_retry = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();
        assert!(!stale_graph_retry.applied);
        assert!(stale_graph_retry.proposals.is_empty());
        assert!(
            stale_graph_retry
                .warnings
                .iter()
                .all(|warning| !warning.contains("FAILED")),
            "warnings: {:?}",
            stale_graph_retry.warnings
        );
    }

    #[test]
    fn consolidate_failed_unlink_stays_destination_published_and_is_not_applied() {
        let (_dir, root, store, mut journal) = journal_fixture();
        publish_consolidation_destination(&root, &journal).unwrap();
        journal.phase = ConsolidationPhase::DestinationPublished;
        let source = root.join(&journal.source_path);
        fs::remove_file(&source).unwrap();
        fs::create_dir(&source).unwrap();
        let journal_path = persist_test_journal(&root, &journal);

        let manifest = memory_consolidate(&store, true, 0.0).unwrap();
        assert!(!manifest.applied);
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("non-file consolidation source")),
            "warnings: {:?}",
            manifest.warnings
        );
        assert_eq!(
            read_test_journal(&journal_path).phase,
            ConsolidationPhase::DestinationPublished
        );
        assert!(root.join(&journal.destination_path).exists());
        assert!(source.is_dir());
    }

    #[test]
    fn consolidate_failed_rewrite_stays_source_removed_and_is_not_applied() {
        let (_dir, root, store, mut journal) = journal_fixture();
        publish_consolidation_destination(&root, &journal).unwrap();
        nestweaver_store::durable_sidecar::remove_file_durable(&root.join(&journal.source_path))
            .unwrap();
        journal.phase = ConsolidationPhase::SourceRemoved;
        journal.rewrite_paths = vec!["missing-rewrite-target.md".to_string()];
        let journal_path = persist_test_journal(&root, &journal);

        let manifest = memory_consolidate(&store, true, 0.0).unwrap();
        assert!(!manifest.applied);
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("wikilink rewrite target")),
            "warnings: {:?}",
            manifest.warnings
        );
        assert_eq!(
            read_test_journal(&journal_path).phase,
            ConsolidationPhase::SourceRemoved
        );
        assert!(root.join(&journal.destination_path).exists());
    }

    #[test]
    fn consolidate_applies_multiple_independent_proposals_as_one_recoverable_batch() {
        let (_dir, root) = make_vault(&[
            ("_logs/2025-01-01.md", "# First log\n"),
            ("_logs/2025-01-02.md", "# Second log\n"),
            (
                "_ideas/idea-a.md",
                "[[_logs/2025-01-01]] and [[_logs/2025-01-02]]\n",
            ),
            (
                "_ideas/idea-b.md",
                "[[_logs/2025-01-01]] and [[_logs/2025-01-02]]\n",
            ),
            (
                "_ideas/idea-c.md",
                "[[_logs/2025-01-01]] and [[_logs/2025-01-02]]\n",
            ),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let manifest = memory_consolidate(&store, true, 4_000_000_000.0).unwrap();
        assert!(manifest.applied, "warnings: {:?}", manifest.warnings);
        assert_eq!(manifest.proposals.len(), 2);
        assert!(root.join("_ideas/2025-01-01.md").exists());
        assert!(root.join("_ideas/2025-01-02.md").exists());
        let idea = fs::read_to_string(root.join("_ideas/idea-a.md")).unwrap();
        assert!(idea.contains("[[_ideas/2025-01-01]]"));
        assert!(idea.contains("[[_ideas/2025-01-02]]"));

        let journals: Vec<_> = fs::read_dir(root.join(CONSOLIDATION_JOURNAL_DIR))
            .unwrap()
            .map(|entry| read_test_journal(&entry.unwrap().path()))
            .collect();
        assert_eq!(journals.len(), 2);
        assert!(
            journals
                .iter()
                .all(|journal| journal.phase == ConsolidationPhase::Complete)
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

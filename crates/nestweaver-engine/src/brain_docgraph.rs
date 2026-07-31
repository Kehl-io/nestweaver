//! Feature F9: first-class document-graph operations over the markdown/vault
//! graph (wikilinks, tags, notes).
//!
//! These functions promote queries that skills previously reimplemented in
//! ad-hoc shell into reusable engine helpers, surfaced as `brain_*` MCP tools
//! and `brain` CLI subcommands. Each is graceful on an empty / no-vault DB:
//! it returns an empty result rather than erroring.

use std::collections::HashMap;

use anyhow::Result;
use nestweaver_store::{BrokenWikilinkRow, GraphStore, NoteLite};
use serde::{Deserialize, Serialize};

use crate::clustering::{self, Graph};

/// Default allowlist of index / MOC notes excluded from orphan detection.
/// Matched case-insensitively against the note's file path and title; entries
/// without an extension (e.g. "MOC") match as a substring of either.
pub const DEFAULT_ORPHAN_ALLOWLIST: &[&str] = &[
    "Projects.md",
    "_brain/index.md",
    "index.md",
    "README.md",
    "MOC",
];

// ── 1. broken links ──────────────────────────────────────────────────────────

/// A wikilink that resolved at less than full confidence, or not at all.
///
/// `resolved_target_uid` is the difference between the two, and it matters:
/// confidence encodes WHICH RESOLVER TIER matched, not how likely the link is
/// to be wrong. A same-folder match scores 0.95 and a unique global
/// filename-stem match scores 0.90 — both are unique, unambiguous resolutions,
/// and the latter is exactly how Obsidian resolves a bare `[[Note]]`. Reporting
/// those as "broken" alongside links that point at nothing told callers that
/// three quarters of a healthy vault was broken (nw-100).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenLink {
    pub source_uid: String,
    pub source_path: String,
    pub wikilink_text: String,
    pub confidence: f32,
    pub suggested_target_uids: Vec<String>,
    /// The note this link actually points at, when it resolved. `None` means
    /// no target exists — the only case that is genuinely broken.
    pub resolved_target_uid: Option<String>,
}

impl BrokenLink {
    /// True when the link points at no note at all.
    pub fn is_unresolved(&self) -> bool {
        self.resolved_target_uid.is_none()
    }
}

/// Find wikilinks that resolved below full confidence OR not at all, pairing
/// each with up to `max_suggestions` candidate note UIDs whose title fuzzily
/// matches the link text (substring / token overlap).
///
/// Callers wanting genuinely-broken links must filter on
/// [`BrokenLink::is_unresolved`] — a sub-1.0 confidence means "matched at a
/// lower resolver tier", not "wrong". See [`BrokenLink`].
pub fn broken_links(store: &GraphStore, max_suggestions: usize) -> Result<Vec<BrokenLink>> {
    let rows: Vec<BrokenWikilinkRow> = store.broken_wikilinks().map_err(|e| anyhow::anyhow!(e))?;
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let notes = store
        .list_notes_lite(None)
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        // Never suggest the note the link is written in. A date-stamped log
        // linking to a date-stamped note matched itself on substring, so the
        // advice read "fix this broken link by linking to itself" (nw-100).
        let suggestions = suggest_targets(&r.wikilink_text, &notes, max_suggestions, &r.source_uid);
        out.push(BrokenLink {
            source_uid: r.source_uid,
            source_path: r.source_path,
            wikilink_text: r.wikilink_text,
            confidence: r.confidence,
            suggested_target_uids: suggestions,
            resolved_target_uid: if r.current_target_uid.is_empty() {
                None
            } else {
                Some(r.current_target_uid)
            },
        });
    }
    Ok(out)
}

/// Rank note UIDs by how well their title matches `text` (case-insensitive
/// substring either direction, with exact match first). Returns at most `max`.
/// Collapse a link target or note key to a comparable form: lowercase, with
/// every run of non-alphanumeric characters reduced to a single space.
///
/// Lets `blast-radius-production-grade` match a note whose stem is written
/// `Blast Radius Production Grade` without resorting to fuzzy distance.
fn normalize_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
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

/// The filename stem of a note's path, lowercased.
fn note_stem(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// Rank note UIDs by how well their TITLE or FILENAME STEM matches `text`.
///
/// Stems matter because Obsidian wikilinks target filenames, and the resolver
/// keys on stems too (its priority 3/3b). This function looked only at titles,
/// so it stayed silent on the one case where a suggestion is most valuable: a
/// target that matches a filename stem shared by SEVERAL notes. The resolver
/// requires a unique stem and so declines to pick, leaving the link unresolved —
/// and with no suggestion, the caller was told nothing at all despite both
/// candidates being known (nw-100).
///
/// Deliberately exact-or-substring, with no edit-distance fuzzing. On the real
/// vault most unresolved links point at targets that do not exist anywhere —
/// backlog IDs that are YAML entries rather than notes, and notes since deleted.
/// Fuzzy matching would manufacture a confident-looking suggestion for every one
/// of them, which is worse than returning none.
fn suggest_targets(text: &str, notes: &[NoteLite], max: usize, source_uid: &str) -> Vec<String> {
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return vec![];
    }
    let needle_norm = normalize_key(&needle);

    let mut scored: Vec<(u8, &NoteLite)> = Vec::new();
    for n in notes {
        if n.uid == source_uid {
            continue;
        }
        let title = n.title.to_lowercase();
        let stem = note_stem(&n.file_path);

        // Exact on either key, raw or normalized.
        let exact = title == needle
            || stem == needle
            || (!needle_norm.is_empty()
                && (normalize_key(&title) == needle_norm || normalize_key(&stem) == needle_norm));

        let score = if exact {
            3
        } else if title.contains(&needle) || (!stem.is_empty() && stem.contains(&needle)) {
            2
        } else if (!title.is_empty() && needle.contains(&title))
            || (!stem.is_empty() && needle.contains(&stem))
        {
            1
        } else {
            0
        };
        if score > 0 {
            scored.push((score, n));
        }
    }
    // Deterministic order: score, then title, then uid — the uid tiebreak keeps
    // two notes sharing a stem AND a title from ordering arbitrarily.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.title.cmp(&b.1.title))
            .then_with(|| a.1.uid.cmp(&b.1.uid))
    });
    scored
        .into_iter()
        .take(max)
        .map(|(_, n)| n.uid.clone())
        .collect()
}

// ── 2. orphan documents ──────────────────────────────────────────────────────

/// A note with no inbound and no outbound wikilinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanDocument {
    pub uid: String,
    pub title: String,
    pub file_path: String,
}

/// Find notes with zero inbound AND zero outbound wikilinks, excluding any
/// note matching the allowlist. `path_prefix` (when set) restricts to notes
/// whose file path starts with it. `vault_uid` (when set) restricts to one
/// vault. `allowlist` defaults to [`DEFAULT_ORPHAN_ALLOWLIST`] when empty.
pub fn orphan_documents(
    store: &GraphStore,
    vault_uid: Option<&str>,
    path_prefix: Option<&str>,
    allowlist: &[String],
) -> Result<Vec<OrphanDocument>> {
    let notes = store
        .list_notes_lite(vault_uid)
        .map_err(|e| anyhow::anyhow!(e))?;
    if notes.is_empty() {
        return Ok(vec![]);
    }
    let with_out = store
        .note_uids_with_outbound_wikilinks()
        .map_err(|e| anyhow::anyhow!(e))?;
    let with_in = store
        .note_uids_with_inbound_wikilinks()
        .map_err(|e| anyhow::anyhow!(e))?;

    let default_allow: Vec<String>;
    let allow: &[String] = if allowlist.is_empty() {
        default_allow = DEFAULT_ORPHAN_ALLOWLIST
            .iter()
            .map(|s| s.to_string())
            .collect();
        &default_allow
    } else {
        allowlist
    };

    let mut out = Vec::new();
    for n in notes {
        if with_out.contains(&n.uid) || with_in.contains(&n.uid) {
            continue;
        }
        if let Some(prefix) = path_prefix
            && !n.file_path.starts_with(prefix)
        {
            continue;
        }
        if is_allowlisted(&n, allow) {
            continue;
        }
        out.push(OrphanDocument {
            uid: n.uid,
            title: n.title,
            file_path: n.file_path,
        });
    }
    out.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    Ok(out)
}

/// True when a note matches an allowlist entry. An entry that looks like a
/// path/filename (contains `.` or `/`) matches case-insensitively as a suffix
/// of the file path or an exact title match; a bare token (e.g. "MOC") matches
/// as a substring of the path or title.
fn is_allowlisted(note: &NoteLite, allowlist: &[String]) -> bool {
    let path_lc = note.file_path.to_lowercase().replace('\\', "/");
    let title_lc = note.title.to_lowercase();
    for entry in allowlist {
        let e = entry.to_lowercase().replace('\\', "/");
        if e.is_empty() {
            continue;
        }
        if e.contains('.') || e.contains('/') {
            if path_lc == e || path_lc.ends_with(&format!("/{e}")) || title_lc == e {
                return true;
            }
        } else if path_lc.contains(&e) || title_lc.contains(&e) {
            return true;
        }
    }
    false
}

// ── 3. topic clusters ────────────────────────────────────────────────────────

/// A community of notes connected by wikilinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicCluster {
    pub cluster_id: u32,
    pub members: Vec<String>,
    pub label: String,
}

/// Run Louvain-style local-moving community detection over the Note↔Note
/// wikilink subgraph and return one cluster per detected community. The label
/// is the title of the highest-PageRank member (falling back to highest
/// wikilink-degree, then the first member). Reuses the SAME
/// [`clustering::leiden`] algorithm the code graph uses (single-level local
/// moving, not full Leiden) — only the loader differs. Empty DB → empty vec.
pub fn topic_clusters(store: &GraphStore, resolution: f64) -> Result<Vec<TopicCluster>> {
    let notes = store
        .list_notes_lite(None)
        .map_err(|e| anyhow::anyhow!(e))?;
    if notes.is_empty() {
        return Ok(vec![]);
    }
    let edges = store
        .note_wikilink_edges()
        .map_err(|e| anyhow::anyhow!(e))?;

    let uid_to_idx: HashMap<&str, usize> = notes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.uid.as_str(), i))
        .collect();
    let n = notes.len();

    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![vec![]; n];
    let mut total_weight = 0.0;
    let mut degree: Vec<usize> = vec![0; n];
    for (src, dst) in &edges {
        if let (Some(&si), Some(&di)) = (uid_to_idx.get(src.as_str()), uid_to_idx.get(dst.as_str()))
        {
            neighbors[si].push((di, 1.0));
            neighbors[di].push((si, 1.0));
            total_weight += 1.0;
            degree[si] += 1;
            degree[di] += 1;
        }
    }

    let graph = Graph {
        n,
        neighbors,
        total_weight,
    };
    let result = clustering::leiden(&graph, resolution, 100);

    let mut clusters: Vec<TopicCluster> = Vec::new();
    for community in &result.communities {
        if community.members.is_empty() {
            continue;
        }
        let label = label_for(&community.members, &notes, &degree);
        let members: Vec<String> = community
            .members
            .iter()
            .map(|&idx| notes[idx].uid.clone())
            .collect();
        clusters.push(TopicCluster {
            cluster_id: community.id,
            members,
            label,
        });
    }
    clusters.sort_by_key(|c| std::cmp::Reverse(c.members.len()));
    Ok(clusters)
}

/// Pick a label for a cluster: title of the highest-PageRank member, breaking
/// ties (e.g. PageRank all zero) by wikilink degree, then by member order.
fn label_for(member_idxs: &[usize], notes: &[NoteLite], degree: &[usize]) -> String {
    member_idxs
        .iter()
        .max_by(|&&a, &&b| {
            notes[a]
                .pagerank_score
                .partial_cmp(&notes[b].pagerank_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| degree[a].cmp(&degree[b]))
        })
        .map(|&idx| notes[idx].title.clone())
        .unwrap_or_default()
}

// ── 4. tag graph ─────────────────────────────────────────────────────────────

/// Co-occurring tag with the number of notes it shares with the focus tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoOccurringTag {
    pub tag: String,
    pub count: usize,
}

/// Tag co-occurrence result for a single focus tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagGraph {
    pub tag: String,
    pub count: usize,
    pub co_occurring: Vec<CoOccurringTag>,
}

/// Compute the focus `tag`'s note count and its co-occurring tags (tags that
/// appear on the same notes), ranked by shared-note count descending. Empty DB
/// or unknown tag → `{tag, count: 0, co_occurring: []}`.
pub fn tag_graph(store: &GraphStore, tag: &str) -> Result<TagGraph> {
    let focus = tag.trim().trim_start_matches('#').to_lowercase();
    let sets = store.note_tag_sets().map_err(|e| anyhow::anyhow!(e))?;
    Ok(tag_graph_from_sets(&focus, &sets))
}

/// Build a [`TagGraph`] for every distinct tag in the vault, sorted by note
/// count descending then tag name ascending. This is the full tag
/// co-occurrence graph in one call, intended for taxonomy-drift detection.
/// Empty DB / no tags → empty vec.
pub fn tag_graph_all(store: &GraphStore) -> Result<Vec<TagGraph>> {
    let sets = store.note_tag_sets().map_err(|e| anyhow::anyhow!(e))?;

    // Collect the distinct (normalized) tags across all notes.
    let mut distinct: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_note, tags) in &sets {
        for t in tags {
            let norm = t.trim().trim_start_matches('#').to_lowercase();
            if !norm.is_empty() && seen.insert(norm.clone()) {
                distinct.push(norm);
            }
        }
    }

    let mut graphs: Vec<TagGraph> = distinct
        .iter()
        .map(|focus| tag_graph_from_sets(focus, &sets))
        .collect();
    graphs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    Ok(graphs)
}

/// Shared co-occurrence computation for a single normalized focus tag against
/// pre-fetched note tag sets. `focus` must already be trimmed/lowercased.
fn tag_graph_from_sets(focus: &str, sets: &[(String, Vec<String>)]) -> TagGraph {
    let mut focus_count = 0usize;
    let mut co: HashMap<String, usize> = HashMap::new();
    for (_note, tags) in sets {
        if !tags.iter().any(|t| t.to_lowercase() == focus) {
            continue;
        }
        focus_count += 1;
        for t in tags {
            if t.to_lowercase() == focus {
                continue;
            }
            *co.entry(t.clone()).or_default() += 1;
        }
    }
    let mut co_occurring: Vec<CoOccurringTag> = co
        .into_iter()
        .map(|(tag, count)| CoOccurringTag { tag, count })
        .collect();
    co_occurring.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));

    TagGraph {
        tag: focus.to_string(),
        count: focus_count,
        co_occurring,
    }
}

// ── 5. doc stats ─────────────────────────────────────────────────────────────

/// A tag with its note count, for the top-tags summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

/// Aggregate statistics over the document graph. All seven keys are always
/// present (empty DB → zeros / empty collections).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocStats {
    pub total_notes: usize,
    pub total_wikilinks: usize,
    /// Links that point at no note at all. This is the vault-health number.
    pub broken_wikilinks: usize,
    /// Links that DID resolve, but below full confidence — a same-folder or
    /// unique filename-stem match rather than a path or unique-title match.
    /// Counted separately because folding them into `broken_wikilinks` reported
    /// 75% of a healthy vault as broken when the real figure was 11% (nw-100).
    pub low_confidence_wikilinks: usize,
    pub orphans: usize,
    pub avg_outdegree: f64,
    pub top_tags: Vec<TagCount>,
    pub notes_by_year: HashMap<String, usize>,
}

/// Compose the other document-graph functions plus counts into a single
/// summary. `top_tags` is capped at `top_tags_limit`. Graceful on empty DB.
pub fn doc_stats(store: &GraphStore, top_tags_limit: usize) -> Result<DocStats> {
    let total_notes = store.count_notes().map_err(|e| anyhow::anyhow!(e))?;
    let total_wikilinks = store
        .count_wikilink_edges()
        .map_err(|e| anyhow::anyhow!(e))?;
    let suspect = broken_links(store, 0)?;
    let broken = suspect.iter().filter(|l| l.is_unresolved()).count();
    let low_confidence = suspect.len() - broken;
    let orphans = orphan_documents(store, None, None, &[])?.len();

    // avg_outdegree: note-level wikilink edges / total notes.
    let note_edges = store
        .note_wikilink_edges()
        .map_err(|e| anyhow::anyhow!(e))?;
    let avg_outdegree = if total_notes == 0 {
        0.0
    } else {
        note_edges.len() as f64 / total_notes as f64
    };

    // top_tags from note tag sets.
    let sets = store.note_tag_sets().map_err(|e| anyhow::anyhow!(e))?;
    let mut tag_counts: HashMap<String, usize> = HashMap::new();
    for (_note, tags) in &sets {
        for t in tags {
            *tag_counts.entry(t.clone()).or_default() += 1;
        }
    }
    let mut top_tags: Vec<TagCount> = tag_counts
        .into_iter()
        .map(|(tag, count)| TagCount { tag, count })
        .collect();
    top_tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    top_tags.truncate(top_tags_limit);

    // notes_by_year from created_at (fallback modified_at) ISO timestamps.
    let mut notes_by_year: HashMap<String, usize> = HashMap::new();
    for note in store.list_notes(None).map_err(|e| anyhow::anyhow!(e))? {
        let ts = note.created_at.as_deref().or(note.modified_at.as_deref());
        if let Some(year) = ts.and_then(|t| t.get(0..4)).filter(|y| y.len() == 4) {
            *notes_by_year.entry(year.to_string()).or_default() += 1;
        }
    }

    Ok(DocStats {
        total_notes,
        total_wikilinks,
        broken_wikilinks: broken,
        low_confidence_wikilinks: low_confidence,
        orphans,
        avg_outdegree,
        top_tags,
        notes_by_year,
    })
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_md::index_markdown_directory_in_memory;
    use std::collections::HashSet as Set;
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

    /// A vault exercising every F9 surface:
    /// - Alpha + Beta link to each other (resolved, high confidence).
    /// - Two notes both titled "Dup"; a link to "Dup" is ambiguous → conf < 1.0
    ///   (a broken/suspect link).
    /// - Island.md links to nothing and nothing links to it → orphan.
    /// - index.md is an allowlisted island → NOT an orphan.
    /// - Alpha + Beta share the #project tag; Beta also has #urgent → tag
    ///   co-occurrence.
    fn f9_vault() -> (tempfile::TempDir, GraphStore) {
        let (dir, root) = make_vault(&[
            (
                "Alpha.md",
                "---\ntags: [project]\n---\n# Alpha\n\nSee [[Beta]] and [[Dup]].\n",
            ),
            (
                "Beta.md",
                "---\ntags: [project, urgent]\n---\n# Beta\n\nBack to [[Alpha]].\n",
            ),
            ("one/Dup.md", "# Dup\n\nfirst dup\n"),
            ("two/Dup.md", "# Dup\n\nsecond dup\n"),
            ("Island.md", "# Island\n\nNo links here.\n"),
            ("index.md", "# Index\n\nA lonely MOC.\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        (dir, store)
    }

    #[test]
    fn broken_links_surfaces_ambiguous_wikilink() {
        let (_dir, store) = f9_vault();
        let broken = broken_links(&store, 5).unwrap();
        // The [[Dup]] link from Alpha is ambiguous (two notes titled "Dup"),
        // so its edges carry confidence < 1.0 and surface as broken.
        assert!(
            !broken.is_empty(),
            "expected at least one broken/ambiguous wikilink, got none"
        );
        let dup = broken
            .iter()
            .find(|b| b.wikilink_text.eq_ignore_ascii_case("Dup"))
            .expect("a broken link for [[Dup]]");
        assert!(dup.confidence < 1.0);
        // Suggestions should include at least one of the two Dup notes.
        assert!(
            !dup.suggested_target_uids.is_empty(),
            "expected suggested targets for [[Dup]]"
        );
    }

    /// nw-100: a link that resolved at a lower tier is NOT broken.
    ///
    /// `[[Sibling]]` in `folder/a.md` resolves to `folder/Sibling.md` by
    /// same-folder match at confidence 0.95 — a unique, unambiguous target, and
    /// how Obsidian itself resolves a bare link. It must carry a
    /// `resolved_target_uid`, and `doc_stats` must not count it as broken.
    #[test]
    fn a_lower_tier_resolution_is_not_broken() {
        let (_dir, root) = make_vault(&[
            (
                "folder/a.md",
                "# A\n\nSee [[Sibling]] and [[Nowhere At All]].\n",
            ),
            ("folder/Sibling.md", "# Different Title Entirely\n\nhi\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let links = broken_links(&store, 5).unwrap();

        let sibling = links
            .iter()
            .find(|b| b.wikilink_text.eq_ignore_ascii_case("Sibling"))
            .expect("[[Sibling]] should appear as a sub-1.0 resolution");
        assert!(
            sibling.confidence < 1.0,
            "same-folder match scores below 1.0"
        );
        assert!(
            !sibling.is_unresolved(),
            "it resolved — it must carry a target, not read as broken: {sibling:?}"
        );
        assert!(sibling.resolved_target_uid.is_some());

        let nowhere = links
            .iter()
            .find(|b| b.wikilink_text.eq_ignore_ascii_case("Nowhere At All"))
            .expect("the dangling link should appear");
        assert!(
            nowhere.is_unresolved(),
            "a link to nothing is the only genuinely broken case"
        );

        // The headline metric must count only the dangling one.
        let stats = doc_stats(&store, 5).unwrap();
        assert_eq!(
            stats.broken_wikilinks, 1,
            "only [[Nowhere At All]] is broken; counting the resolved one is the nw-100 defect"
        );
        assert!(
            stats.low_confidence_wikilinks >= 1,
            "the lower-tier resolution must still be reported, just not as broken"
        );
    }

    fn note_lite(uid: &str, title: &str, file_path: &str) -> NoteLite {
        NoteLite {
            uid: uid.to_string(),
            title: title.to_string(),
            file_path: file_path.to_string(),
            vault_uid: "vault:test".to_string(),
            pagerank_score: 0.0,
        }
    }

    /// nw-100: the case where a suggestion is worth most.
    ///
    /// Two notes share the filename stem `blast-radius-production-grade`, so the
    /// resolver's stem tier requires uniqueness and declines to pick — the link
    /// is correctly unresolved. But both candidates are known, and the
    /// title-only suggester returned NOTHING because neither title resembles the
    /// stem. A human could disambiguate instantly if shown them.
    #[test]
    fn suggests_both_notes_that_share_a_filename_stem() {
        let notes = vec![
            note_lite(
                "note:backlog",
                "Blast Radius → production-grade for enterprise code review",
                "Workspaces/NestWeaver/backlog/blast-radius-production-grade.md",
            ),
            note_lite(
                "note:prd",
                "Blast Radius → Production-Grade — PRD",
                "Workspaces/NestWeaver/notes/2026-07/prd/blast-radius-production-grade.md",
            ),
            note_lite("note:other", "Unrelated", "misc/unrelated.md"),
        ];

        let got = suggest_targets("blast-radius-production-grade", &notes, 5, "note:src");

        assert!(got.contains(&"note:backlog".to_string()), "got: {got:?}");
        assert!(got.contains(&"note:prd".to_string()), "got: {got:?}");
        assert!(
            !got.contains(&"note:other".to_string()),
            "must not drag in unrelated notes: {got:?}"
        );
    }

    /// Separator style must not matter: a hyphenated target should match a note
    /// whose stem uses spaces, and vice versa.
    #[test]
    fn suggestion_matching_ignores_separator_style() {
        let notes = vec![note_lite(
            "note:a",
            "Some Other Title",
            "notes/Phase B Execution Index.md",
        )];
        let got = suggest_targets("phase-b-execution-index", &notes, 5, "note:src");
        assert_eq!(got, vec!["note:a".to_string()], "got: {got:?}");
    }

    /// The restraint matters as much as the recall. Most unresolved links on the
    /// real vault point at targets that exist NOWHERE — backlog IDs that are YAML
    /// entries rather than notes, and notes since deleted. Returning a
    /// confident-looking guess for those is worse than returning none, so there
    /// is deliberately no edit-distance fuzzing.
    #[test]
    fn a_target_that_exists_nowhere_gets_no_suggestion() {
        let notes = vec![
            note_lite(
                "note:a",
                "Daemon Architecture",
                "notes/daemon-architecture.md",
            ),
            note_lite("note:b", "Release Process", "notes/release-process.md"),
        ];
        for absent in ["nw-092", "server-mode-phase1-transport", "zzz"] {
            let got = suggest_targets(absent, &notes, 5, "note:src");
            assert!(
                got.is_empty(),
                "{absent:?} exists nowhere — a guess is worse than nothing, got: {got:?}"
            );
        }
    }

    /// nw-100: never advise fixing a link by pointing it at its own source.
    #[test]
    fn suggestions_never_include_the_source_note() {
        let notes = vec![
            NoteLite {
                uid: "note:self".to_string(),
                title: "Daily Log 2026-07-27".to_string(),
                file_path: "_logs/2026-07-27.md".to_string(),
                vault_uid: "vault:test".to_string(),
                pagerank_score: 0.0,
            },
            NoteLite {
                uid: "note:other".to_string(),
                title: "Daily Log 2026-07-27 Review".to_string(),
                file_path: "notes/review.md".to_string(),
                vault_uid: "vault:test".to_string(),
                pagerank_score: 0.0,
            },
        ];
        let got = suggest_targets("Daily Log 2026-07-27", &notes, 5, "note:self");
        assert!(
            !got.contains(&"note:self".to_string()),
            "must not suggest the source note itself, got: {got:?}"
        );
        assert!(got.contains(&"note:other".to_string()));
    }

    #[test]
    fn broken_links_includes_unresolved_and_dedups_ambiguous() {
        // BUG repro: a vault with a genuinely-unresolved [[GhostLink]] (no
        // target note) AND an ambiguous [[Dup]] (two notes titled "Dup").
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\nSee [[GhostLink]] and [[Dup]].\n"),
            ("one/Dup.md", "# Dup\n\nfirst dup\n"),
            ("two/Dup.md", "# Dup\n\nsecond dup\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let broken = broken_links(&store, 5).unwrap();

        // GhostLink resolves to no note → must surface as broken.
        let ghost = broken
            .iter()
            .find(|b| b.wikilink_text.eq_ignore_ascii_case("GhostLink"));
        assert!(
            ghost.is_some(),
            "genuinely-unresolved [[GhostLink]] must surface as broken, got: {:?}",
            broken.iter().map(|b| &b.wikilink_text).collect::<Vec<_>>()
        );

        // The ambiguous [[Dup]] must collapse to exactly ONE row, not one per
        // candidate note.
        let dup_rows: Vec<_> = broken
            .iter()
            .filter(|b| b.wikilink_text.eq_ignore_ascii_case("Dup"))
            .collect();
        assert_eq!(
            dup_rows.len(),
            1,
            "ambiguous [[Dup]] must produce exactly one row, got {}",
            dup_rows.len()
        );

        // doc-stats broken count must be > 0.
        let stats = doc_stats(&store, 5).unwrap();
        assert!(
            stats.broken_wikilinks > 0,
            "doc-stats broken_wikilinks must be > 0"
        );
    }

    #[test]
    fn orphans_list_island_but_not_allowlisted_index() {
        let (_dir, store) = f9_vault();
        let orphans = orphan_documents(&store, None, None, &[]).unwrap();
        let titles: Set<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
        assert!(titles.contains("Island"), "Island should be an orphan");
        assert!(
            !titles.contains("Index"),
            "allowlisted index.md must not be an orphan"
        );
        // Alpha/Beta are linked, so never orphans.
        assert!(!titles.contains("Alpha"));
        assert!(!titles.contains("Beta"));
    }

    #[test]
    fn tag_graph_computes_co_occurrence() {
        let (_dir, store) = f9_vault();
        let tg = tag_graph(&store, "project").unwrap();
        assert_eq!(tg.tag, "project");
        assert_eq!(tg.count, 2, "two notes carry #project");
        // #urgent co-occurs with #project on Beta.
        let urgent = tg
            .co_occurring
            .iter()
            .find(|c| c.tag == "urgent")
            .expect("urgent should co-occur with project");
        assert_eq!(urgent.count, 1);
    }

    #[test]
    fn tag_graph_all_returns_every_tag() {
        // Two notes: one tagged {#a, #b}, one tagged {#a}.
        let (_dir, root) = make_vault(&[
            ("One.md", "---\ntags: [a, b]\n---\n# One\n"),
            ("Two.md", "---\ntags: [a]\n---\n# Two\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let graphs = tag_graph_all(&store).unwrap();
        assert_eq!(graphs.len(), 2, "expected exactly two distinct tags");

        // Sorted by count desc, name asc → a (2) before b (1).
        let a = &graphs[0];
        assert_eq!(a.tag, "a");
        assert_eq!(a.count, 2, "two notes carry #a");
        let a_b = a
            .co_occurring
            .iter()
            .find(|c| c.tag == "b")
            .expect("b co-occurs with a");
        assert_eq!(a_b.count, 1);

        let b = &graphs[1];
        assert_eq!(b.tag, "b");
        assert_eq!(b.count, 1, "one note carries #b");
        let b_a = b
            .co_occurring
            .iter()
            .find(|c| c.tag == "a")
            .expect("a co-occurs with b");
        assert_eq!(b_a.count, 1);
    }

    #[test]
    fn tag_graph_all_empty_db_is_graceful() {
        let store = GraphStore::in_memory().unwrap();
        assert!(tag_graph_all(&store).unwrap().is_empty());
    }

    #[test]
    fn doc_stats_returns_all_seven_keys() {
        let (_dir, store) = f9_vault();
        let stats = doc_stats(&store, 10).unwrap();
        // Serialize and assert the seven keys exist (graceful, all present).
        let v = serde_json::to_value(&stats).unwrap();
        for key in [
            "total_notes",
            "total_wikilinks",
            "broken_wikilinks",
            "orphans",
            "avg_outdegree",
            "top_tags",
            "notes_by_year",
        ] {
            assert!(v.get(key).is_some(), "missing doc_stats key: {key}");
        }
        assert_eq!(stats.total_notes, 6);
        assert!(stats.orphans >= 1, "Island is an orphan");
        assert!(stats.total_wikilinks > 0);
    }

    #[test]
    fn topic_clusters_groups_linked_notes() {
        let (_dir, store) = f9_vault();
        let clusters = topic_clusters(&store, 0.5).unwrap();
        // Alpha<->Beta are mutually linked; they should land in one cluster
        // whose label is one of their titles.
        let alpha_cluster = clusters.iter().find(|c| {
            c.members.iter().any(|m| m.contains("Alpha")) || c.label == "Alpha" || c.label == "Beta"
        });
        assert!(
            alpha_cluster.is_some(),
            "expected a cluster containing the Alpha/Beta component"
        );
    }

    #[test]
    fn empty_db_is_graceful() {
        let store = GraphStore::in_memory().unwrap();
        assert!(broken_links(&store, 5).unwrap().is_empty());
        assert!(
            orphan_documents(&store, None, None, &[])
                .unwrap()
                .is_empty()
        );
        assert!(topic_clusters(&store, 0.5).unwrap().is_empty());
        let tg = tag_graph(&store, "anything").unwrap();
        assert_eq!(tg.count, 0);
        assert!(tg.co_occurring.is_empty());
        let stats = doc_stats(&store, 10).unwrap();
        assert_eq!(stats.total_notes, 0);
        assert_eq!(stats.orphans, 0);
        assert_eq!(stats.avg_outdegree, 0.0);
    }
}

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

/// A broken (low-confidence) wikilink with suggested resolution targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenLink {
    pub source_uid: String,
    pub source_path: String,
    pub wikilink_text: String,
    pub confidence: f32,
    pub suggested_target_uids: Vec<String>,
}

/// Find wikilink edges whose target resolution is suspect (confidence < 1.0),
/// pairing each with up to `max_suggestions` candidate note UIDs whose title
/// fuzzily matches the link text (substring / token overlap).
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
        let suggestions = suggest_targets(&r.wikilink_text, &notes, max_suggestions);
        out.push(BrokenLink {
            source_uid: r.source_uid,
            source_path: r.source_path,
            wikilink_text: r.wikilink_text,
            confidence: r.confidence,
            suggested_target_uids: suggestions,
        });
    }
    Ok(out)
}

/// Rank note UIDs by how well their title matches `text` (case-insensitive
/// substring either direction, with exact match first). Returns at most `max`.
fn suggest_targets(text: &str, notes: &[NoteLite], max: usize) -> Vec<String> {
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return vec![];
    }
    let mut scored: Vec<(u8, &NoteLite)> = Vec::new();
    for n in notes {
        let title = n.title.to_lowercase();
        let score = if title == needle {
            3
        } else if title.contains(&needle) {
            2
        } else if needle.contains(&title) && !title.is_empty() {
            1
        } else {
            0
        };
        if score > 0 {
            scored.push((score, n));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
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

/// Run Leiden community detection over the Note↔Note wikilink subgraph and
/// return one cluster per detected community. The label is the title of the
/// highest-PageRank member (falling back to highest wikilink-degree, then the
/// first member). Reuses the SAME [`clustering::leiden`] algorithm the code
/// graph uses — only the loader differs. Empty DB → empty vec.
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
    pub broken_wikilinks: usize,
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
    let broken = broken_links(store, 0)?.len();
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

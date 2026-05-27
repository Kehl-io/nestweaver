//! Hierarchical code summaries for token-efficient retrieval.
//!
//! Generates deterministic, compact summaries at three levels of the code graph:
//!
//! - **Symbol**: one-line summary of a function/class with callers and callees
//! - **File**: list of exports and their kinds, plus import sources
//! - **Cluster**: community-level description with key types and dependencies
//!
//! These summaries are generated entirely from graph data — no LLM needed.
//! They are designed to give an LLM maximum architectural understanding per
//! token, following the HCGS (Hierarchical Code Graph Summarization) approach.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

// ── Types ────────────────────────────────────────────────────────────────────

/// The granularity of a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryLevel {
    Symbol,
    File,
    Cluster,
}

impl std::fmt::Display for SummaryLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryLevel::Symbol => write!(f, "symbol"),
            SummaryLevel::File => write!(f, "file"),
            SummaryLevel::Cluster => write!(f, "cluster"),
        }
    }
}

impl std::str::FromStr for SummaryLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "symbol" => Ok(SummaryLevel::Symbol),
            "file" => Ok(SummaryLevel::File),
            "cluster" => Ok(SummaryLevel::Cluster),
            other => Err(format!(
                "unknown summary level '{}': expected symbol, file, or cluster",
                other
            )),
        }
    }
}

/// A single deterministic summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub level: SummaryLevel,
    pub target_uid: String,
    pub target_name: String,
    pub content: String,
    /// Estimated token count (chars / 4).
    pub token_estimate: usize,
}

/// Persisted collection of summaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryStore {
    pub summaries: Vec<Summary>,
}

// ── Generation ───────────────────────────────────────────────────────────────

/// Generate summaries at the given level from the graph store.
pub fn generate_summaries(store: &GraphStore, level: SummaryLevel) -> Result<Vec<Summary>> {
    match level {
        SummaryLevel::Symbol => generate_symbol_summaries(store),
        SummaryLevel::File => generate_file_summaries(store),
        SummaryLevel::Cluster => generate_cluster_summaries(store),
    }
}

/// Symbol-level: one-line summary per function/class.
///
/// Format: `{kind} {name}({params}) -> {return} | callers: [list] | callees: [list] | file: {path}:{line}`
fn generate_symbol_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    let symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    let mut summaries = Vec::with_capacity(symbols.len());

    for sym in &symbols {
        let callers = store.callers_of(&sym.uid).unwrap_or_default();
        let callees = store.callees_of(&sym.uid).unwrap_or_default();

        let caller_names: Vec<&str> = callers.iter().map(|c| c.name.as_str()).collect();
        let callee_names: Vec<&str> = callees.iter().map(|c| c.name.as_str()).collect();

        let content = format!(
            "{kind} {sig} | callers: [{callers}] | callees: [{callees}] | file: {file}:{line}",
            kind = sym.kind,
            sig = sym.signature,
            callers = caller_names.join(", "),
            callees = callee_names.join(", "),
            file = sym.file_path,
            line = sym.start_line,
        );

        let token_estimate = content.len().div_ceil(4);

        summaries.push(Summary {
            level: SummaryLevel::Symbol,
            target_uid: sym.uid.clone(),
            target_name: sym.name.clone(),
            content,
            token_estimate,
        });
    }

    // Sort by content string for deterministic output.
    summaries.sort_by(|a, b| a.content.cmp(&b.content));

    Ok(summaries)
}

/// File-level: exports and import sources per file.
///
/// Format: `{file}: exports {n} symbols: {name1} ({kind}), ... | imports from: [files]`
fn generate_file_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    let symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    // Group symbols by file path.
    let mut by_file: BTreeMap<String, Vec<&nestweaver_schema::Symbol>> = BTreeMap::new();
    for sym in &symbols {
        by_file.entry(sym.file_path.clone()).or_default().push(sym);
    }

    // Build a set of file paths that each file imports from, by checking
    // CALLS edges. If symbol A in file X calls symbol B in file Y, then
    // file X imports from file Y.
    let (_, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!("load_code_symbols_and_edges: {e}"))?;

    // Build uid -> file_path map.
    let uid_to_file: HashMap<&str, &str> = symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.file_path.as_str()))
        .collect();

    // file_path -> set of file_paths it depends on.
    let mut imports_from: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (src_uid, dst_uid, _) in &edges {
        if let (Some(&src_file), Some(&dst_file)) = (
            uid_to_file.get(src_uid.as_str()),
            uid_to_file.get(dst_uid.as_str()),
        ) && src_file != dst_file
        {
            imports_from.entry(src_file).or_default().insert(dst_file);
        }
    }

    let mut summaries = Vec::with_capacity(by_file.len());

    for (file_path, file_symbols) in &by_file {
        // Sort symbols by start_line for deterministic output.
        let mut sorted_syms: Vec<&&nestweaver_schema::Symbol> = file_symbols.iter().collect();
        sorted_syms.sort_by_key(|s| s.start_line);

        let export_list: Vec<String> = sorted_syms
            .iter()
            .map(|s| format!("{} ({})", s.name, s.kind))
            .collect();

        let import_files: Vec<&str> = imports_from
            .get(file_path.as_str())
            .map(|set| {
                let mut v: Vec<&str> = set.iter().copied().collect();
                v.sort();
                v
            })
            .unwrap_or_default();

        let content = format!(
            "{file}: exports {n} symbols: {exports} | imports from: [{imports}]",
            file = file_path,
            n = file_symbols.len(),
            exports = export_list.join(", "),
            imports = import_files.join(", "),
        );

        let token_estimate = content.len().div_ceil(4);

        // Use file path as both UID and name (files don't have a separate UID
        // in the same sense symbols do — we use the path as the identity key).
        summaries.push(Summary {
            level: SummaryLevel::File,
            target_uid: file_path.clone(),
            target_name: file_path.clone(),
            content,
            token_estimate,
        });
    }

    Ok(summaries)
}

/// Cluster-level: community description with key types and cross-cluster deps.
///
/// Format: `Cluster {id} ({name}, {n} symbols): key types: [{top symbols}] | files: [{file list}] | depends on: [other clusters]`
fn generate_cluster_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    let output = crate::cluster_dispatch::compute_clusters(store, 1.0)?;

    if output.communities.is_empty() {
        return Ok(vec![]);
    }

    // Build symbol UID -> cluster ID mapping for cross-cluster dependency detection.
    let mut uid_to_cluster: HashMap<String, u32> = HashMap::new();
    for community in &output.communities {
        for member in &community.members {
            uid_to_cluster.insert(member.uid.clone(), community.id);
        }
    }

    // Load edges to find cross-cluster dependencies.
    let (_, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!("load_code_symbols_and_edges: {e}"))?;

    // cluster_id -> set of cluster_ids it depends on (outgoing edges to other clusters).
    let mut cluster_deps: HashMap<u32, HashSet<u32>> = HashMap::new();
    for (src_uid, dst_uid, _) in &edges {
        if let (Some(&src_cluster), Some(&dst_cluster)) =
            (uid_to_cluster.get(src_uid), uid_to_cluster.get(dst_uid))
            && src_cluster != dst_cluster
        {
            cluster_deps
                .entry(src_cluster)
                .or_default()
                .insert(dst_cluster);
        }
    }

    // Build a name lookup: cluster_id -> cluster_name.
    let id_to_name: HashMap<u32, &str> = output
        .communities
        .iter()
        .map(|c| (c.id, c.name.as_str()))
        .collect();

    let mut summaries = Vec::with_capacity(output.communities.len());

    for community in &output.communities {
        // Key types: top 5 symbols by PageRank (or just the first 5 alphabetically).
        let mut key_members = community.members.clone();
        key_members.truncate(5);
        let key_types: Vec<String> = key_members
            .iter()
            .map(|m| format!("{} ({})", m.name, m.kind))
            .collect();

        // Dependent cluster names.
        let dep_names: Vec<String> = cluster_deps
            .get(&community.id)
            .map(|deps| {
                let mut names: Vec<String> = deps
                    .iter()
                    .filter_map(|id| id_to_name.get(id).map(|n| n.to_string()))
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();

        let content = format!(
            "Cluster {id} ({name}, {n} symbols): key types: [{keys}] | files: [{files}] | depends on: [{deps}]",
            id = community.id,
            name = community.name,
            n = community.member_count,
            keys = key_types.join(", "),
            files = community.key_files.join(", "),
            deps = dep_names.join(", "),
        );

        let token_estimate = content.len().div_ceil(4);

        summaries.push(Summary {
            level: SummaryLevel::Cluster,
            target_uid: format!("cluster:{}", community.id),
            target_name: community.name.clone(),
            content,
            token_estimate,
        });
    }

    Ok(summaries)
}

// ── Sidecar persistence ──────────────────────────────────────────────────────

/// Compute the sidecar file path: `<db>.summaries.json`.
///
/// Uses `OsStr::push` to preserve the `.lbug` extension, producing e.g.
/// `test.lbug.summaries.json`.
pub fn sidecar_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".summaries.json")
}

/// Save summaries to the sidecar file.
pub fn save_summaries(db_path: &Path, summaries: &[Summary]) -> Result<()> {
    let path = sidecar_path(db_path);
    let store = SummaryStore {
        summaries: summaries.to_vec(),
    };
    let json = serde_json::to_string_pretty(&store).context("failed to serialize summaries")?;
    fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Load summaries from the sidecar file. Returns `Ok(None)` when the sidecar
/// does not exist.
pub fn load_summaries(db_path: &Path) -> Result<Option<Vec<Summary>>> {
    crate::migrate_sidecar(db_path, "summaries.json", ".summaries.json");
    let path = sidecar_path(db_path);
    if !path.exists() {
        return Ok(None);
    }
    let json =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let store: SummaryStore =
        serde_json::from_str(&json).context("failed to parse summaries sidecar")?;
    Ok(Some(store.summaries))
}

// ── Token-budget truncation ──────────────────────────────────────────────────

/// Truncate a list of summaries to fit within a token budget.
///
/// Returns the subset of summaries that fits, in order.
pub fn truncate_to_budget(summaries: &[Summary], token_budget: usize) -> Vec<&Summary> {
    let mut used = 0usize;
    let mut result = Vec::new();
    for s in summaries {
        if used + s.token_estimate > token_budget {
            break;
        }
        used += s.token_estimate;
        result.push(s);
    }
    result
}

/// Filter summaries by target name or UID.
pub fn filter_by_target<'a>(summaries: &'a [Summary], target: &str) -> Vec<&'a Summary> {
    let needle = target.to_lowercase();
    summaries
        .iter()
        .filter(|s| {
            s.target_uid.to_lowercase().contains(&needle)
                || s.target_name.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Render summaries as plain text, one per line.
pub fn render_text(summaries: &[Summary]) -> String {
    summaries
        .iter()
        .map(|s| s.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_level_roundtrip() {
        for level in [
            SummaryLevel::Symbol,
            SummaryLevel::File,
            SummaryLevel::Cluster,
        ] {
            let s = level.to_string();
            let parsed: SummaryLevel = s.parse().unwrap();
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn summary_level_parse_error() {
        let result: Result<SummaryLevel, _> = "invalid".parse();
        assert!(result.is_err());
    }

    #[test]
    fn truncate_to_budget_respects_limit() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "a".to_string(),
                target_name: "fn_a".to_string(),
                content: "x".repeat(40), // 10 tokens
                token_estimate: 10,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "b".to_string(),
                target_name: "fn_b".to_string(),
                content: "y".repeat(40),
                token_estimate: 10,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "c".to_string(),
                target_name: "fn_c".to_string(),
                content: "z".repeat(40),
                token_estimate: 10,
            },
        ];

        let result = truncate_to_budget(&summaries, 25);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].target_uid, "a");
        assert_eq!(result[1].target_uid, "b");
    }

    #[test]
    fn truncate_to_budget_empty_budget() {
        let summaries = vec![Summary {
            level: SummaryLevel::Symbol,
            target_uid: "a".to_string(),
            target_name: "fn_a".to_string(),
            content: "x".repeat(40),
            token_estimate: 10,
        }];

        let result = truncate_to_budget(&summaries, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn filter_by_target_matches_name() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::File,
                target_uid: "src/auth.rs".to_string(),
                target_name: "src/auth.rs".to_string(),
                content: "auth stuff".to_string(),
                token_estimate: 3,
            },
            Summary {
                level: SummaryLevel::File,
                target_uid: "src/main.rs".to_string(),
                target_name: "src/main.rs".to_string(),
                content: "main stuff".to_string(),
                token_estimate: 3,
            },
        ];

        let result = filter_by_target(&summaries, "auth");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target_name, "src/auth.rs");
    }

    #[test]
    fn render_text_joins_lines() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "a".to_string(),
                target_name: "a".to_string(),
                content: "line one".to_string(),
                token_estimate: 2,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "b".to_string(),
                target_name: "b".to_string(),
                content: "line two".to_string(),
                token_estimate: 2,
            },
        ];

        let text = render_text(&summaries);
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn symbol_summaries_from_store() {
        let store = GraphStore::in_memory().unwrap();
        let sym = nestweaver_schema::Symbol {
            uid: "sym:test:abc:10".to_string(),
            name: "greet".to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/main.js".to_string(),
            start_line: 10,
            signature: "function greet(name)".to_string(),
            summary: None,
            content_hash: "abc".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        store.insert_symbol(&sym).unwrap();

        let summaries = generate_symbol_summaries(&store).unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].content.contains("greet"));
        assert!(summaries[0].content.contains("src/main.js:10"));
        assert_eq!(summaries[0].level, SummaryLevel::Symbol);
        assert!(summaries[0].token_estimate > 0);
    }

    #[test]
    fn file_summaries_from_store() {
        let store = GraphStore::in_memory().unwrap();
        let sym1 = nestweaver_schema::Symbol {
            uid: "sym:test:abc:1".to_string(),
            name: "foo".to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            signature: "fn foo()".to_string(),
            summary: None,
            content_hash: "a".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        let sym2 = nestweaver_schema::Symbol {
            uid: "sym:test:abc:20".to_string(),
            name: "bar".to_string(),
            kind: nestweaver_schema::SymbolKind::Class,
            repo_uid: "repo:test".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 20,
            signature: "class Bar".to_string(),
            summary: None,
            content_hash: "b".to_string(),
            embedding: None,
            pagerank_score: Some(0.3),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        store.insert_symbol(&sym1).unwrap();
        store.insert_symbol(&sym2).unwrap();

        let summaries = generate_file_summaries(&store).unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].content.contains("src/lib.rs"));
        assert!(summaries[0].content.contains("exports 2 symbols"));
        assert!(summaries[0].content.contains("foo (Function)"));
        assert!(summaries[0].content.contains("bar (Class)"));
    }

    #[test]
    fn sidecar_path_appends_suffix() {
        let db = Path::new("/tmp/test.lbug");
        let expected = PathBuf::from("/tmp/test.lbug.summaries.json");
        assert_eq!(sidecar_path(db), expected);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        let summaries = vec![Summary {
            level: SummaryLevel::File,
            target_uid: "src/main.rs".to_string(),
            target_name: "src/main.rs".to_string(),
            content: "src/main.rs: exports 1 symbols: main (Function) | imports from: []"
                .to_string(),
            token_estimate: 17,
        }];

        save_summaries(&db_path, &summaries).unwrap();
        let loaded = load_summaries(&db_path).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].target_uid, "src/main.rs");
        assert_eq!(loaded[0].content, summaries[0].content);
    }

    #[test]
    fn load_summaries_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent.lbug");
        let result = load_summaries(&db_path).unwrap();
        assert!(result.is_none());
    }
}

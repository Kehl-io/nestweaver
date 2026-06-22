// Process tracing: discovers execution flows from entry points and
// computes change-impact analysis across traced processes.

use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

/// A traced execution process rooted at an entry-point symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResult {
    pub uid: String,
    pub name: String,
    pub entry_point_uid: String,
    pub repo_uid: String,
    pub depth: u32,
    pub symbol_count: u32,
    pub members: Vec<ProcessMember>,
}

/// A symbol participating in a traced process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMember {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub call_depth: u32,
}

/// Impact of a set of changed files on the code graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeImpact {
    pub affected_symbols: Vec<AffectedSymbol>,
    pub affected_processes: Vec<AffectedProcess>,
    pub risk: RiskLevel,
    pub blast_radius: usize,
}

/// A symbol affected by a file change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub uid: String,
    pub name: String,
    pub file_path: String,
}

/// A process affected by a file change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedProcess {
    pub name: String,
    pub uid: String,
    pub affected_symbol_count: u32,
    pub total_symbol_count: u32,
}

/// Risk level derived from the number of affected processes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Heuristic entry-point names — symbols with these names are treated as
/// entry points when no explicit `is_entry_point` flag is present.
const ENTRY_POINT_NAMES: &[&str] = &[
    "main", "handler", "handle", "run", "start", "execute", "serve", "init",
];

/// Pre-computed suffixes for entry-point name matching (e.g. "_main", "_handler").
const ENTRY_POINT_SUFFIXES: &[&str] = &[
    "_main", "_handler", "_handle", "_run", "_start", "_execute", "_serve", "_init",
];

/// Trace all execution processes in the graph store.
///
/// An "entry point" is a symbol that has `is_entry_point == true`, matches
/// a heuristic name (main, handler, etc.), or has zero callers (root node).
///
/// For each entry point, performs BFS forward through `callees_of` to
/// discover all reachable symbols, producing a `ProcessResult`.
pub fn trace_processes(store: &GraphStore, max_depth: u32) -> Result<Vec<ProcessResult>> {
    let symbols = store
        .list_all_symbols()
        .context("list_all_symbols for process tracing")?;

    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    let has_caller = store
        .all_callee_uids()
        .context("all_callee_uids for process tracing")?;

    let entry_points: Vec<&nestweaver_schema::Symbol> = symbols
        .iter()
        .filter(|sym| {
            if sym.is_entry_point {
                return true;
            }
            let name_lower = sym.name.to_lowercase();
            let is_heuristic_entry = ENTRY_POINT_NAMES.iter().any(|ep| name_lower == *ep)
                || ENTRY_POINT_SUFFIXES
                    .iter()
                    .any(|suffix| name_lower.ends_with(suffix));
            let is_root = !has_caller.contains(&sym.uid);
            is_heuristic_entry || is_root
        })
        .collect();

    let mut processes = Vec::new();
    for ep in entry_points {
        let process = trace_single_process(store, ep, max_depth)?;
        processes.push(process);
    }

    Ok(processes)
}

/// BFS forward from a single entry point to build one `ProcessResult`.
fn trace_single_process(
    store: &GraphStore,
    entry_point: &nestweaver_schema::Symbol,
    max_depth: u32,
) -> Result<ProcessResult> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(entry_point.uid.clone());

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((entry_point.uid.clone(), 0));

    let mut members = Vec::new();
    let mut deepest: u32 = 0;

    // Entry point is member at depth 0.
    members.push(ProcessMember {
        uid: entry_point.uid.clone(),
        name: entry_point.name.clone(),
        file_path: entry_point.file_path.clone(),
        call_depth: 0,
    });

    while let Some((current_uid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        let callees = store.callees_of(&current_uid).unwrap_or_default();
        for callee in callees {
            if visited.contains(&callee.uid) {
                continue;
            }
            visited.insert(callee.uid.clone());
            let member_depth = depth + 1;
            if member_depth > deepest {
                deepest = member_depth;
            }
            members.push(ProcessMember {
                uid: callee.uid.clone(),
                name: callee.name.clone(),
                file_path: callee.file_path.clone(),
                call_depth: member_depth,
            });
            queue.push_back((callee.uid.clone(), member_depth));
        }
    }

    // UID = hash of entry_point_uid for determinism.
    let uid = {
        let key = format!("process:{}", entry_point.uid);
        let hash = crate::hash::blake3_hex(&key);
        format!("proc:{}", &hash[..16])
    };

    let name = derive_process_name(entry_point);

    Ok(ProcessResult {
        uid,
        name,
        entry_point_uid: entry_point.uid.clone(),
        repo_uid: entry_point.repo_uid.clone(),
        depth: deepest,
        symbol_count: members.len() as u32,
        members,
    })
}

fn derive_process_name(entry_point: &nestweaver_schema::Symbol) -> String {
    use nestweaver_schema::EntryPointKind;
    match entry_point.entry_point_kind {
        Some(EntryPointKind::HttpHandler) => {
            let base = entry_point
                .file_path
                .rsplit('/')
                .next()
                .unwrap_or(&entry_point.file_path);
            format!("http:{base}::{}", entry_point.name)
        }
        Some(EntryPointKind::Main) => {
            let dir = entry_point.file_path.rsplit('/').nth(1).unwrap_or("root");
            format!("{dir}::main")
        }
        Some(EntryPointKind::TestEntry) => format!("test::{}", entry_point.name),
        Some(EntryPointKind::LambdaHandler) => format!("lambda::{}", entry_point.name),
        Some(EntryPointKind::EventListener) => format!("event::{}", entry_point.name),
        Some(EntryPointKind::CronJob) => format!("cron::{}", entry_point.name),
        Some(EntryPointKind::CliCommand) => format!("cli::{}", entry_point.name),
        None => format!("process::{}", entry_point.name),
    }
}

/// Detect the impact of changed files on traced processes.
///
/// 1. Maps changed file paths to affected symbols via `symbols_in_file`.
/// 2. Traces all processes in the store.
/// 3. Cross-references to find which processes contain affected symbols.
/// 4. Assigns a risk level: High (>3 processes), Medium (1-3), Low (0).
pub fn detect_changes_impact(
    store: &GraphStore,
    changed_files: &[String],
    max_depth: u32,
) -> Result<ChangeImpact> {
    // Step 1: collect all symbols in changed files.
    let mut affected_symbols = Vec::new();
    let mut affected_uids: HashSet<String> = HashSet::new();

    for file_path in changed_files {
        let syms = store.symbols_in_file(file_path).unwrap_or_default();
        for sym in syms {
            if affected_uids.insert(sym.uid.clone()) {
                affected_symbols.push(AffectedSymbol {
                    uid: sym.uid.clone(),
                    name: sym.name.clone(),
                    file_path: sym.file_path.clone(),
                });
            }
        }
    }

    // Step 2: trace processes.
    let processes = trace_processes(store, max_depth)?;

    // Step 3: cross-reference.
    let mut affected_processes = Vec::new();
    for proc in &processes {
        let member_uids: HashSet<&str> = proc.members.iter().map(|m| m.uid.as_str()).collect();
        let overlap: u32 = affected_uids
            .iter()
            .filter(|uid| member_uids.contains(uid.as_str()))
            .count() as u32;
        if overlap > 0 {
            affected_processes.push(AffectedProcess {
                name: proc.name.clone(),
                uid: proc.uid.clone(),
                affected_symbol_count: overlap,
                total_symbol_count: proc.symbol_count,
            });
        }
    }

    // Step 4: risk level.
    let risk = match affected_processes.len() {
        0 => RiskLevel::Low,
        1..=3 => RiskLevel::Medium,
        _ => RiskLevel::High,
    };

    let blast_radius = affected_symbols.len() + affected_processes.len();

    Ok(ChangeImpact {
        affected_symbols,
        affected_processes,
        risk,
        blast_radius,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_processes_returns_empty_for_empty_store() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let result = trace_processes(&store, 10).expect("trace_processes");
        assert!(result.is_empty());
    }

    #[test]
    fn detect_changes_impact_returns_low_risk_for_unknown_files() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let impact = detect_changes_impact(&store, &["nonexistent/file.rs".to_string()], 10)
            .expect("detect_changes_impact");
        assert_eq!(impact.risk, RiskLevel::Low);
        assert!(impact.affected_symbols.is_empty());
        assert!(impact.affected_processes.is_empty());
    }

    #[test]
    fn trace_processes_finds_entry_point_by_name() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        // Create a "main" symbol and a callee.
        let main_sym = Symbol {
            uid: "sym:main".to_string(),
            name: "main".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/main.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn main()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: true,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        let helper_sym = Symbol {
            uid: "sym:helper".to_string(),
            name: "helper".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 5,
            end_line: 5,
            signature: "fn helper()".to_string(),
            summary: None,
            content_hash: "h2".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };

        store.insert_symbol(&main_sym).expect("insert main");
        store.insert_symbol(&helper_sym).expect("insert helper");

        let edge = ResolvedEdge {
            source_uid: "sym:main".to_string(),
            target_uid: "sym:helper".to_string(),
            edge_type: EdgeType::Calls,
            confidence: 0.9,
            link_type: None,
            evidence: vec![],
        };
        store.insert_edge(&edge).expect("insert edge");

        let processes = trace_processes(&store, 10).expect("trace_processes");

        // "main" should be detected as entry point and the process should include helper.
        let main_proc = processes.iter().find(|p| p.name == "process::main");
        assert!(main_proc.is_some(), "expected a process rooted at 'main'");
        let proc = main_proc.unwrap();
        assert_eq!(proc.symbol_count, 2);
        assert!(proc.members.iter().any(|m| m.name == "helper"));
    }

    #[test]
    fn detect_changes_impact_finds_affected_process() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        let main_sym = Symbol {
            uid: "sym:main".to_string(),
            name: "main".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/main.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn main()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: true,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        let helper_sym = Symbol {
            uid: "sym:helper".to_string(),
            name: "helper".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 5,
            end_line: 5,
            signature: "fn helper()".to_string(),
            summary: None,
            content_hash: "h2".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };

        store.insert_symbol(&main_sym).expect("insert main");
        store.insert_symbol(&helper_sym).expect("insert helper");
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:main".to_string(),
                target_uid: "sym:helper".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .expect("insert edge");

        let impact = detect_changes_impact(&store, &["src/lib.rs".to_string()], 10)
            .expect("detect_changes_impact");

        assert_eq!(impact.risk, RiskLevel::Medium);
        assert!(!impact.affected_symbols.is_empty());
        assert!(impact.affected_symbols.iter().any(|s| s.name == "helper"),);
        assert!(!impact.affected_processes.is_empty());
    }
}

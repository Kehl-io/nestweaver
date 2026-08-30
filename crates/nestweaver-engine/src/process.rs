// Process tracing: discovers execution flows from entry points and
// computes change-impact analysis across traced processes.

use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

use crate::blast_radius::{AnalysisStatus, GateState, Notification, NotificationLevel};

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
    #[serde(default)]
    pub status: AnalysisStatus,
    #[serde(default)]
    pub notifications: Vec<Notification>,
    #[serde(default)]
    pub gate_state: GateState,
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
        .filter(|sym| symbol_is_entry_point(sym, &has_caller))
        .collect();

    let mut processes = Vec::new();
    for ep in entry_points {
        let process = trace_single_process(store, ep, max_depth)?;
        processes.push(process);
    }

    Ok(processes)
}

/// Whether a symbol is treated as a process entry point: explicitly flagged, a
/// heuristic entry name/suffix, or a root (no callers). `has_caller` is the set
/// of uids that appear as a callee somewhere (i.e. have at least one caller).
fn symbol_is_entry_point(sym: &nestweaver_schema::Symbol, has_caller: &HashSet<String>) -> bool {
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
}

/// Edge types that constitute a forward "process" step — the same set
/// `callees_of` traverses. The reverse-reachability that scopes change impact
/// MUST use the reverse of exactly this set, or it would miss entry points that
/// reach a changed symbol through a non-CALLS edge.
const PROCESS_EDGE_TYPES: [&str; 3] = ["CALLS", "IMPORTS", "CROSS_REPO_LINK"];

/// In-memory forward adjacency over [`PROCESS_EDGE_TYPES`]. Prebuilt once from a
/// bulk edge load so process tracing needs no per-node DB round-trips.
type Adjacency = std::collections::HashMap<String, Vec<String>>;

/// Breadth-first reachability over a prebuilt adjacency, from every seed up to
/// `max_depth` hops (seeds included at depth 0). Pure in-memory — O(edges).
fn reachable_in_memory(
    adj: &Adjacency,
    seeds: &HashSet<String>,
    max_depth: u32,
) -> HashSet<String> {
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut queue: VecDeque<(String, u32)> = seeds.iter().map(|u| (u.clone(), 0u32)).collect();
    while let Some((uid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        if let Some(neighbors) = adj.get(&uid) {
            for n in neighbors {
                if visited.insert(n.clone()) {
                    queue.push_back((n.clone(), depth + 1));
                }
            }
        }
    }
    visited
}

/// Forward trace of one entry point over a prebuilt adjacency — the in-memory
/// twin of [`trace_single_process`], with member metadata looked up from
/// `sym_by_uid`.
fn trace_single_process_in_memory(
    entry_point: &nestweaver_schema::Symbol,
    fwd_adj: &Adjacency,
    sym_by_uid: &std::collections::HashMap<String, &nestweaver_schema::Symbol>,
    max_depth: u32,
) -> ProcessResult {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(entry_point.uid.clone());
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((entry_point.uid.clone(), 0));

    let mut members = vec![ProcessMember {
        uid: entry_point.uid.clone(),
        name: entry_point.name.clone(),
        file_path: entry_point.file_path.clone(),
        call_depth: 0,
    }];
    let mut deepest: u32 = 0;

    while let Some((current_uid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let Some(callees) = fwd_adj.get(&current_uid) else {
            continue;
        };
        for callee in callees {
            if !visited.insert(callee.clone()) {
                continue;
            }
            let member_depth = depth + 1;
            deepest = deepest.max(member_depth);
            // Prefer rich metadata; fall back to the uid if the callee isn't in
            // the symbol table (e.g. an unresolved foreign leaf).
            let (name, file_path) = sym_by_uid
                .get(callee)
                .map(|s| (s.name.clone(), s.file_path.clone()))
                .unwrap_or_else(|| (callee.clone(), String::new()));
            members.push(ProcessMember {
                uid: callee.clone(),
                name,
                file_path,
                call_depth: member_depth,
            });
            queue.push_back((callee.clone(), member_depth));
        }
    }
    // Deterministic member order regardless of adjacency iteration.
    members.sort_by(|a, b| (a.call_depth, &a.uid).cmp(&(b.call_depth, &b.uid)));

    let uid = {
        let key = format!("process:{}", entry_point.uid);
        let hash = crate::hash::blake3_hex(&key);
        format!("proc:{}", &hash[..16])
    };
    ProcessResult {
        uid,
        name: derive_process_name(entry_point),
        entry_point_uid: entry_point.uid.clone(),
        repo_uid: entry_point.repo_uid.clone(),
        depth: deepest,
        symbol_count: members.len() as u32,
        members,
    }
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
/// 2. Finds ONLY the processes whose entry point can reach an affected symbol
///    (reverse-BFS from the affected set, intersected with entry points), then
///    forward-traces just those — bounded by the blast radius, not the whole
///    graph. This is equivalent to tracing every process and keeping the ones
///    that overlap the affected set, but without the O(all-entry-points × BFS)
///    scan that made this hang on a large store.
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
    let mut status = AnalysisStatus::Complete;
    let mut notifications = Vec::new();
    let mut unassessed = Vec::new();

    for file_path in changed_files {
        let syms = match store.symbols_in_file(file_path) {
            Ok(syms) => syms,
            Err(e) => {
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!("mapping changed file {file_path} to symbols failed: {e}"),
                    descriptor: "store.symbols-in-file-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
                continue;
            }
        };
        if syms.is_empty()
            && nestweaver_parser::detect_language(std::path::Path::new(file_path)).is_some()
        {
            unassessed.push(file_path.as_str());
        }
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
    if !unassessed.is_empty() {
        status = status.max(AnalysisStatus::Partial);
        notifications.push(Notification {
            level: NotificationLevel::Warning,
            message: format!(
                "changed source file(s) with no indexed symbols (new file, stale index, or path \
                 drift) — their impact was not assessed: {}",
                unassessed.join(", ")
            ),
            descriptor: "changed-file-no-symbols".to_string(),
        });
    }

    // Early out: no changed file mapped to an indexed symbol → nothing to trace.
    if affected_uids.is_empty() {
        let risk = RiskLevel::Low;
        let gate_state = crate::blast_radius::derive_gate_state(status, risk);
        return Ok(ChangeImpact {
            affected_symbols,
            affected_processes: Vec::new(),
            risk,
            blast_radius: 0,
            status,
            notifications,
            gate_state,
        });
    }

    // Step 2 (scoped, in-memory): identify entry points that can reach an
    // affected symbol, then forward-trace only those. A process overlaps the
    // affected set iff its entry point reaches an affected symbol within
    // `max_depth` — exactly the entry points found by a reverse-BFS from the
    // affected symbols over the process edge set.
    //
    // The whole graph is loaded once via a handful of bulk queries and both the
    // reverse and forward walks run in memory, so cost is O(edges) and constant
    // in the number of processes — instead of the old O(entry-points × BFS) with
    // a DB round-trip per visited node, which hung on a large store.
    //
    // nw-354. The scan TOLERATES an undecodable row (nw-335) rather than losing
    // the whole corpus to it, so `Ok` does not mean "complete". Take the
    // integrity with the rows: a dropped symbol row removes an entry point,
    // which shrinks `affected_processes`, which lowers `RiskLevel` — and
    // `derive_gate_state` then reports `GateState::Ok`. A row nobody could read
    // must never make a change look SAFER. `74f82da0` wired the six callers
    // that make a completeness claim and named this one as the follow-up; this
    // is that follow-up, reusing its descriptor rather than minting a second
    // string for one condition.
    let symbols = match store.list_all_symbols_with_integrity() {
        Ok((symbols, integrity)) => {
            if let Some(disclosure) = integrity.disclosure() {
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!(
                        "the symbol graph could not be read completely, so this \
                         blast radius is a FLOOR and cannot justify a merge: {disclosure}"
                    ),
                    descriptor: "store.list-symbols-incomplete".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
            }
            symbols
        }
        // Unchanged behaviour: a scan that FAILED still fails the analysis.
        Err(e) => {
            return Err(anyhow::Error::new(e).context("list_all_symbols for change impact"));
        }
    };
    let sym_by_uid: std::collections::HashMap<String, &nestweaver_schema::Symbol> =
        symbols.iter().map(|s| (s.uid.clone(), s)).collect();

    let typed_edges = store
        .load_typed_edges()
        .context("load_typed_edges for change impact")?;
    let mut fwd_adj: Adjacency = std::collections::HashMap::new();
    let mut rev_adj: Adjacency = std::collections::HashMap::new();
    // `has_caller` mirrors `all_callee_uids`: targets of CALLS edges. Used by the
    // entry-point predicate's root test.
    let mut has_caller: HashSet<String> = HashSet::new();
    for edge in &typed_edges {
        let (src, dst, etype) = (&edge.0, &edge.1, edge.2.as_str());
        if etype == "CALLS" {
            has_caller.insert(dst.clone());
        }
        if PROCESS_EDGE_TYPES.contains(&etype) {
            fwd_adj.entry(src.clone()).or_default().push(dst.clone());
            rev_adj.entry(dst.clone()).or_default().push(src.clone());
        }
    }

    // Reverse-reachable ancestors of the affected symbols, then keep the entry
    // points among them (deterministic order via the symbols list).
    let ancestors = reachable_in_memory(&rev_adj, &affected_uids, max_depth);
    let relevant_entry_points: Vec<&nestweaver_schema::Symbol> = symbols
        .iter()
        .filter(|sym| ancestors.contains(&sym.uid) && symbol_is_entry_point(sym, &has_caller))
        .collect();
    let processes: Vec<ProcessResult> = relevant_entry_points
        .iter()
        .map(|ep| trace_single_process_in_memory(ep, &fwd_adj, &sym_by_uid, max_depth))
        .collect();

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

    // Deterministic output order (entry-point iteration order is not stable).
    affected_processes.sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));

    // Step 4: risk level.
    let risk = match affected_processes.len() {
        0 => RiskLevel::Low,
        1..=3 => RiskLevel::Medium,
        _ => RiskLevel::High,
    };

    let blast_radius = affected_symbols.len() + affected_processes.len();
    let gate_state = crate::blast_radius::derive_gate_state(status, risk);

    Ok(ChangeImpact {
        affected_symbols,
        affected_processes,
        risk,
        blast_radius,
        status,
        notifications,
        gate_state,
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
    fn detect_changes_impact_marks_unknown_source_incomplete() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let impact = detect_changes_impact(&store, &["nonexistent/file.rs".to_string()], 10)
            .expect("detect_changes_impact");
        assert_eq!(impact.risk, RiskLevel::Low);
        assert_eq!(impact.status, AnalysisStatus::Partial);
        assert_eq!(impact.gate_state, GateState::DegradedUnknown);
        assert!(
            impact
                .notifications
                .iter()
                .any(|n| n.descriptor == "changed-file-no-symbols")
        );
        assert!(impact.affected_symbols.is_empty());
        assert!(impact.affected_processes.is_empty());
    }

    #[test]
    fn detect_changes_impact_ignores_zero_symbol_non_source_files() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let impact = detect_changes_impact(&store, &["README.md".to_string()], 10)
            .expect("detect_changes_impact");

        assert_eq!(impact.status, AnalysisStatus::Complete);
        assert_eq!(impact.gate_state, GateState::Ok);
        assert!(
            !impact
                .notifications
                .iter()
                .any(|n| n.descriptor == "changed-file-no-symbols")
        );
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
            canonical_id: None,
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
            canonical_id: None,
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
            canonical_id: None,
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
            canonical_id: None,
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

    #[test]
    fn detect_changes_impact_scopes_to_affected_blast_radius() {
        // nw-078: the scoped tracer must report ONLY processes whose entry point
        // reaches an affected symbol, and never fan out over unrelated processes.
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str, entry: bool| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: entry,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        // Chain 1: entryA -> mid -> target (target lives in the changed file).
        // Chain 2: entryB -> other (entirely unrelated to the change).
        for s in [
            mk("sym:entryA", "entryA", "src/a.rs", true),
            mk("sym:mid", "mid", "src/mid.rs", false),
            mk("sym:target", "target", "src/target.rs", false),
            mk("sym:entryB", "entryB", "src/b.rs", true),
            mk("sym:other", "other", "src/other.rs", false),
        ] {
            store.insert_symbol(&s).unwrap();
        }
        for (src, dst) in [
            ("sym:entryA", "sym:mid"),
            ("sym:mid", "sym:target"),
            ("sym:entryB", "sym:other"),
        ] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: dst.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let impact = detect_changes_impact(&store, &["src/target.rs".to_string()], 10)
            .expect("detect_changes_impact");

        // Exactly one affected process — the one rooted at entryA that reaches
        // target. entryB's process must NOT appear (it doesn't reach target).
        assert_eq!(impact.affected_processes.len(), 1);
        assert!(
            impact.affected_processes[0].name.contains("entryA"),
            "expected the entryA process, got {}",
            impact.affected_processes[0].name
        );
        assert!(
            impact
                .affected_processes
                .iter()
                .all(|p| !p.name.contains("entryB")),
            "unrelated entryB process must not be reported"
        );
    }

    /// nw-354. `74f82da0` gave the whole-corpus scans a `ScanIntegrity` and
    /// wired every caller that makes a completeness or safety claim.
    /// `detect_changes_impact` was audited and left. It is the one that feeds a
    /// GATE: an undecodable symbol row is invisible to `list_all_symbols`, so
    /// an entry point disappears, the affected-process count falls, the
    /// `RiskLevel` falls with it, and `derive_gate_state` reports `Ok`. A row
    /// nobody could read must never make a change look safer.
    #[test]
    fn a_degraded_symbol_scan_cannot_report_a_clean_gate() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str, sig: &str, entry: bool| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: sig.to_string(),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: entry,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        // Four entry points, all reaching the changed file. entry3's signature
        // carries the embedded NUL that `extract_string` treats as
        // storage-engine partial-scan corruption, so the scan drops its row.
        for s in [
            mk(
                "sym:target",
                "target",
                "src/target.rs",
                "fn target()",
                false,
            ),
            mk("sym:e0", "e0", "src/e0.rs", "fn e0()", true),
            mk("sym:e1", "e1", "src/e1.rs", "fn e1()", true),
            mk("sym:e2", "e2", "src/e2.rs", "fn e2()", true),
            mk("sym:e3", "e3", "src/e3.rs", "fn e\u{0}3()", true),
        ] {
            store.insert_symbol(&s).unwrap();
        }
        for src in ["sym:e0", "sym:e1", "sym:e2", "sym:e3"] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: "sym:target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let impact = detect_changes_impact(&store, &["src/target.rs".to_string()], 10)
            .expect("detect_changes_impact");

        assert_ne!(
            impact.gate_state,
            GateState::Ok,
            "a scan that dropped a row reported a CLEAN gate — a degraded \
             corpus made the change look safer"
        );
        assert_eq!(impact.status, AnalysisStatus::Degraded);
        assert!(
            impact
                .notifications
                .iter()
                .any(|n| n.descriptor == "store.list-symbols-incomplete"),
            "the caller must be TOLD, not just the log: {:?}",
            impact.notifications
        );
    }

    /// The counterweight `74f82da0` demanded for every wiring: a CLEAN scan
    /// must not degrade, or the signal would fire always and be worthless.
    #[test]
    fn a_clean_symbol_scan_still_reports_a_clean_gate() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str, entry: bool| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: entry,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store
            .insert_symbol(&mk("sym:e0", "e0", "src/e0.rs", true))
            .unwrap();
        store
            .insert_symbol(&mk("sym:target", "target", "src/target.rs", false))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:e0".to_string(),
                target_uid: "sym:target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let impact = detect_changes_impact(&store, &["src/target.rs".to_string()], 10)
            .expect("detect_changes_impact");
        assert_eq!(impact.status, AnalysisStatus::Complete);
        assert_eq!(impact.gate_state, GateState::Ok);
        assert!(
            !impact
                .notifications
                .iter()
                .any(|n| n.descriptor == "store.list-symbols-incomplete")
        );
    }
}

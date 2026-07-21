//! Feature F13: static, source-call-graph-based regression test selection (RTS).
//!
//! Given a set of changed files (or a base git ref), this maps them to the
//! symbols they define, reverse-traverses the CALLS/IMPORTS graph to find which
//! test symbols (transitively) depend on the changed code, and buckets the
//! resulting test files into priority tiers.
//!
//! ## Honesty / limitations
//!
//! This is a *static* selection over the parsed call graph. It is a prioritized
//! signal, NOT a provably-safe test subset. It will miss tests that reach the
//! changed code via mechanisms the static graph does not model:
//!
//!   - reflection / dynamic dispatch / `getattr`-style invocation
//!   - dependency injection / service locators / IoC containers
//!   - code generation and macros expanded at build time
//!   - data-driven / fixture-driven tests, golden files, snapshots
//!   - integration/e2e tests that exercise behavior over the wire
//!
//! Therefore: **"no tests found" does NOT mean it is safe to skip testing.**
//! Treat the output as a ranked starting point for an MR's test run, and keep a
//! periodic full test run in CI to catch what static analysis cannot see.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use nestweaver_parser::is_test_file;
use nestweaver_store::GraphStore;

use crate::blast_radius::{AnalysisStatus, Notification, NotificationLevel};

/// Maximum reverse-traversal depth for finding dependent tests.
const MAX_TEST_DEPTH: u32 = 3;

/// Minimum edge confidence for a reverse-dependency edge to count.
///
/// Mirrors the conservative-but-inclusive threshold used by blast-radius/PR
/// impact: low enough to catch real edges, high enough to drop noise.
const MIN_CONFIDENCE: f32 = 0.3;

/// A test symbol (with its containing file) selected for a given tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedTestSymbol {
    /// UID of the test symbol.
    pub symbol_uid: String,
    /// Name of the test symbol (e.g. test function name).
    pub name: String,
    /// File the test lives in.
    pub test_file: String,
    /// Reverse-traversal depth at which this test was reached (1..=3).
    pub depth: u32,
    /// Edge type by which it was reached (CALLS/IMPORTS/...).
    pub edge_type: String,
    /// Confidence of the edge by which it was reached.
    pub confidence: f32,
}

/// One test file in a tier, grouped with the test symbols it contributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedTestFile {
    /// The test file path.
    pub test_file: String,
    /// Names of the test symbols in this file selected for this tier.
    pub tests: Vec<String>,
    /// A representative symbol UID (highest-confidence test in this file/tier).
    pub symbol_uid: String,
    /// Highest edge confidence among this file's tests in this tier.
    pub confidence: f32,
}

/// The full affected-tests result, bucketed by priority tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedTestsResult {
    /// The changed files that drove the analysis.
    pub changed_files: Vec<String>,
    /// The changed source symbols (defined in the changed files).
    pub changed_symbols: Vec<ChangedSymbolRef>,
    /// Tier 1: a test directly references a changed symbol (depth 1).
    pub tier_1: Vec<AffectedTestFile>,
    /// Tier 2: a test of a depth-1 caller of a changed symbol (depth 2).
    pub tier_2: Vec<AffectedTestFile>,
    /// Tier 3: transitively reachable tests (depth 3).
    pub tier_3: Vec<AffectedTestFile>,
    /// Human-readable summary, e.g. "2 tier-1, 1 tier-2, 0 tier-3 tests affected".
    pub summary: String,
    /// Honest framing of what this analysis can and cannot guarantee.
    pub disclaimer: String,
    /// Whether the selection ran to completion. A `Degraded` status means the
    /// affected-tests set is incomplete — a CI consumer should fall back to
    /// running the full suite rather than trusting this subset.
    #[serde(default)]
    pub status: AnalysisStatus,
    /// Machine-readable reasons the selection was incomplete or degraded.
    #[serde(default)]
    pub notifications: Vec<Notification>,
    /// Machine-readable CI directive derived from `status` (TIA-style
    /// fail-safe widening): any non-Complete run says "run-full-suite" so a
    /// pipeline can act on degradation without parsing notifications.
    /// Values: "selection-usable" | "run-full-suite".
    #[serde(default)]
    pub recommendation: String,
    /// In-band measured-recall disclosure (nw-037): present only when the
    /// rts-eval loop has >= 10 joined (selection, truth) pairs. Absence means
    /// "no measured claim", never "perfect".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<crate::rts_eval::MeasuredRecall>,
}

/// A changed source symbol reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedSymbolRef {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    /// Owning repo of the changed symbol (feeds selection recording and
    /// multi-repo consumers). Empty on records serialized before this field.
    #[serde(default)]
    pub repo_uid: String,
}

const DISCLAIMER: &str = "Static call-graph regression test selection: a prioritized signal, NOT a \
provably-safe subset. Misses reflection, DI, codegen, and data-driven/integration tests. \
\"No tests found\" does not mean safe-to-skip — keep a periodic full test run.";

/// Fail-safe widening (Microsoft TIA precedent): an incomplete selection is
/// only safe to act on by running the FULL suite; never narrow on degradation.
pub(crate) fn derive_recommendation(status: AnalysisStatus) -> &'static str {
    match status {
        AnalysisStatus::Complete => "selection-usable",
        _ => "run-full-suite",
    }
}

/// Compute the affected tests for a set of changed files.
///
/// Pipeline:
///   1. changed_files → changed_symbols (direct `symbols_in_file` lookup).
///   2. for each changed symbol, reverse-traverse CALLS/IMPORTS to depth 3.
///   3. keep reached symbols whose file is a test file (`is_test_file`).
///   4. bucket by traversal depth (tier_1 = depth 1, etc.), ordering within a
///      tier by edge confidence.
pub fn affected_tests(store: &GraphStore, changed_files: &[String]) -> Result<AffectedTestsResult> {
    // Trust core: a failed traversal must surface as `Degraded` so a CI
    // consumer runs the full suite instead of trusting an incomplete subset.
    let mut status = AnalysisStatus::Complete;
    let mut notifications: Vec<Notification> = Vec::new();

    // Step 1: changed files → changed symbols, via direct per-file lookup.
    //
    // Deliberately NOT `detect_changes_impact`: that helper also runs
    // `trace_processes` — a BFS from every entry-point-ish symbol across the
    // ENTIRE store — whose process output this analysis never used. On large
    // multi-repo stores that traversal hangs or crashes the native store
    // layer (the 2.5.10 affected_tests daemon-crash/segfault report).
    let mut changed_symbols: Vec<ChangedSymbolRef> = Vec::new();
    let mut changed_uids: HashSet<String> = HashSet::new();
    // Changed files that resolved to zero indexed symbols (new file or stale
    // index) — non-test source files are disclosed as unassessed; test files
    // are ALWAYS included in tier 1 below (TIA/Develocity always-include rule).
    let mut files_without_symbols: Vec<&String> = Vec::new();
    for file_path in changed_files {
        match store.symbols_in_file(file_path) {
            Ok(syms) => {
                if syms.is_empty() {
                    files_without_symbols.push(file_path);
                }
                for s in syms {
                    if changed_uids.insert(s.uid.clone()) {
                        changed_symbols.push(ChangedSymbolRef {
                            uid: s.uid,
                            name: s.name,
                            file_path: s.file_path,
                            repo_uid: s.repo_uid,
                        });
                    }
                }
            }
            Err(e) => {
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!("mapping changed file {file_path} to symbols failed: {e}"),
                    descriptor: "store.symbols-in-file-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
            }
        }
    }

    // Step 2 + 3: reverse-traverse from each changed symbol and keep tests.
    //
    // A reached test may be discovered from several changed symbols at different
    // depths; we keep the *shallowest* depth (highest priority) for each test,
    // and among equal depths the highest-confidence edge.
    let mut best: HashMap<String, AffectedTestSymbol> = HashMap::new();

    // A changed symbol may itself live in a test file (a test was edited).
    // Such tests are directly affected — treat them as tier 1 (depth 1).
    for cs in &changed_symbols {
        if is_test_file(&cs.file_path) {
            consider(
                &mut best,
                AffectedTestSymbol {
                    symbol_uid: cs.uid.clone(),
                    name: cs.name.clone(),
                    test_file: cs.file_path.clone(),
                    depth: 1,
                    edge_type: "CHANGED".to_string(),
                    confidence: 1.0,
                },
            );
        }
    }

    for cs in &changed_symbols {
        let callers = match store.impact(&cs.uid, MAX_TEST_DEPTH, MIN_CONFIDENCE) {
            Ok(callers) => callers,
            Err(e) => {
                // Do NOT propagate/drop silently — an incomplete affected-tests
                // set that reads as "few tests" is the dangerous failure mode.
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!("reverse traversal for {} failed: {e}", cs.uid),
                    descriptor: "store.impact-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
                continue;
            }
        };
        for node in callers {
            if !is_test_file(&node.file_path) {
                continue;
            }
            consider(
                &mut best,
                AffectedTestSymbol {
                    symbol_uid: node.uid,
                    name: node.name,
                    test_file: node.file_path,
                    depth: node.depth,
                    edge_type: node.edge_type,
                    confidence: node.confidence,
                },
            );
        }
    }

    // Step 4: bucket by depth and group by file.
    let mut tier_1_syms = Vec::new();
    let mut tier_2_syms = Vec::new();
    let mut tier_3_syms = Vec::new();
    for sym in best.into_values() {
        match sym.depth {
            0 | 1 => tier_1_syms.push(sym),
            2 => tier_2_syms.push(sym),
            _ => tier_3_syms.push(sym),
        }
    }

    let mut tier_1 = group_by_file(tier_1_syms);
    let tier_2 = group_by_file(tier_2_syms);
    let tier_3 = group_by_file(tier_3_syms);

    // nw-064: always-include + disclosure for changed files the index doesn't
    // know (new file or stale index). A changed TEST file goes straight into
    // tier 1 — TIA includes newly added tests, Develocity always selects
    // "recently new/changed" tests; missing them during the stale-index window
    // was a silent under-selection. A changed non-test SOURCE file is
    // disclosed as unassessed (mirrors blast_radius's changed-file-no-symbols
    // honesty) without gating — new files are common.
    let selected_files: HashSet<&str> = tier_1
        .iter()
        .chain(&tier_2)
        .chain(&tier_3)
        .map(|f| f.test_file.as_str())
        .collect();
    let mut always_included: Vec<String> = Vec::new();
    let mut unassessed: Vec<&str> = Vec::new();
    for file_path in files_without_symbols {
        if selected_files.contains(file_path.as_str()) {
            continue;
        }
        if is_test_file(file_path) {
            always_included.push(file_path.clone());
        } else if nestweaver_parser::detect_language(std::path::Path::new(file_path)).is_some() {
            unassessed.push(file_path);
        }
    }
    for file_path in always_included.iter() {
        tier_1.push(AffectedTestFile {
            test_file: file_path.clone(),
            tests: Vec::new(),
            symbol_uid: String::new(),
            confidence: 1.0,
        });
    }
    if !always_included.is_empty() {
        notifications.push(Notification {
            level: NotificationLevel::Note,
            message: format!(
                "always-included {} changed test file(s) not yet in the index                  (new test or stale index): {}",
                always_included.len(),
                always_included.join(", ")
            ),
            descriptor: "always-include-changed-test".to_string(),
        });
    }
    if !unassessed.is_empty() {
        notifications.push(Notification {
            level: NotificationLevel::Note,
            message: format!(
                "changed source file(s) with no indexed symbols (new file or stale                  index) — their impact was not assessed: {}",
                unassessed.join(", ")
            ),
            descriptor: "changed-file-no-symbols".to_string(),
        });
    }

    let summary = format!(
        "{} tier-1, {} tier-2, {} tier-3 tests affected",
        count_tests(&tier_1),
        count_tests(&tier_2),
        count_tests(&tier_3),
    );

    Ok(AffectedTestsResult {
        changed_files: changed_files.to_vec(),
        changed_symbols,
        tier_1,
        tier_2,
        tier_3,
        summary,
        disclaimer: DISCLAIMER.to_string(),
        recommendation: derive_recommendation(status).to_string(),
        status,
        notifications,
        measured: None,
    })
}

/// Record `candidate` as the best-known selection for its symbol UID, keeping
/// the shallowest depth and (tie-break) the highest confidence.
fn consider(best: &mut HashMap<String, AffectedTestSymbol>, candidate: AffectedTestSymbol) {
    match best.get(&candidate.symbol_uid) {
        Some(existing)
            if existing.depth < candidate.depth
                || (existing.depth == candidate.depth
                    && existing.confidence >= candidate.confidence) => {}
        _ => {
            best.insert(candidate.symbol_uid.clone(), candidate);
        }
    }
}

/// Group selected test symbols by their test file, ordering files (and the
/// representative confidence) by descending edge confidence.
fn group_by_file(mut syms: Vec<AffectedTestSymbol>) -> Vec<AffectedTestFile> {
    // Sort symbols by confidence desc so the first per file is the strongest.
    syms.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.test_file.cmp(&b.test_file))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut order: Vec<String> = Vec::new();
    let mut by_file: HashMap<String, AffectedTestFile> = HashMap::new();
    let mut seen_tests: HashSet<(String, String)> = HashSet::new();

    for sym in syms {
        let entry = by_file.entry(sym.test_file.clone()).or_insert_with(|| {
            order.push(sym.test_file.clone());
            AffectedTestFile {
                test_file: sym.test_file.clone(),
                tests: Vec::new(),
                symbol_uid: sym.symbol_uid.clone(),
                confidence: sym.confidence,
            }
        });
        if seen_tests.insert((sym.test_file.clone(), sym.name.clone())) {
            entry.tests.push(sym.name.clone());
        }
        if sym.confidence > entry.confidence {
            entry.confidence = sym.confidence;
            entry.symbol_uid = sym.symbol_uid.clone();
        }
    }

    // Emit in discovery order (already confidence-ranked because syms were sorted).
    order
        .into_iter()
        .filter_map(|f| by_file.remove(&f))
        .collect()
}

fn count_tests(files: &[AffectedTestFile]) -> usize {
    files.iter().map(|f| f.tests.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

    fn sym(uid: &str, name: &str, file: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h:{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn edge(src: &str, dst: &str) -> ResolvedEdge {
        ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: dst.to_string(),
            edge_type: EdgeType::Calls,
            confidence: 0.9,
            link_type: None,
            evidence: vec![],
        }
    }

    #[test]
    fn empty_store_yields_empty_tiers() {
        let store = GraphStore::in_memory().expect("store");
        let result = affected_tests(&store, &["src/missing.rs".to_string()]).expect("ok");
        assert!(result.tier_1.is_empty());
        assert!(result.tier_2.is_empty());
        assert!(result.tier_3.is_empty());
        assert_eq!(
            result.summary,
            "0 tier-1, 0 tier-2, 0 tier-3 tests affected"
        );
        assert!(!result.disclaimer.is_empty());
    }

    #[test]
    fn tiers_by_depth_and_excludes_non_test_callers() {
        let store = GraphStore::in_memory().expect("store");

        // Changed source symbol.
        let changed = sym("sym:changed", "computeTotal", "src/billing.rs");
        // A direct test of the changed symbol → tier 1.
        let direct_test = sym("sym:direct_test", "computes_total", "src/billing.test.rs");
        // A non-test caller (depth 1) — must be EXCLUDED from any tier.
        let caller = sym("sym:caller", "invoice", "src/invoice.rs");
        // A test of that caller (reached at depth 2) → tier 2.
        let caller_test = sym("sym:caller_test", "renders_invoice", "src/invoice.test.rs");

        for s in [&changed, &direct_test, &caller, &caller_test] {
            store.insert_symbol(s).expect("insert");
        }

        // direct_test -> changed   (test references changed symbol directly)
        store
            .insert_edge(&edge("sym:direct_test", "sym:changed"))
            .expect("e1");
        // caller -> changed         (non-test caller depends on changed symbol)
        store
            .insert_edge(&edge("sym:caller", "sym:changed"))
            .expect("e2");
        // caller_test -> caller     (test of the caller)
        store
            .insert_edge(&edge("sym:caller_test", "sym:caller"))
            .expect("e3");

        let result = affected_tests(&store, &["src/billing.rs".to_string()]).expect("ok");

        // Changed symbol detected.
        assert!(
            result
                .changed_symbols
                .iter()
                .any(|c| c.name == "computeTotal")
        );

        // Tier 1 contains the direct test, and only it.
        let t1_files: Vec<&str> = result.tier_1.iter().map(|f| f.test_file.as_str()).collect();
        assert_eq!(t1_files, vec!["src/billing.test.rs"], "tier_1 files");
        assert!(
            result.tier_1[0]
                .tests
                .contains(&"computes_total".to_string())
        );

        // Tier 2 contains the caller's test.
        let t2_files: Vec<&str> = result.tier_2.iter().map(|f| f.test_file.as_str()).collect();
        assert_eq!(t2_files, vec!["src/invoice.test.rs"], "tier_2 files");
        assert!(
            result.tier_2[0]
                .tests
                .contains(&"renders_invoice".to_string())
        );

        // The non-test caller must not appear anywhere.
        let all_files: Vec<&str> = result
            .tier_1
            .iter()
            .chain(&result.tier_2)
            .chain(&result.tier_3)
            .map(|f| f.test_file.as_str())
            .collect();
        assert!(
            !all_files.contains(&"src/invoice.rs"),
            "non-test caller src/invoice.rs leaked into tiers: {all_files:?}"
        );

        assert_eq!(
            result.summary,
            "1 tier-1, 1 tier-2, 0 tier-3 tests affected"
        );
    }

    #[test]
    fn jest_style_test_calling_changed_symbol_is_tier_1() {
        // End-to-end: a Jest/Vitest-style `test('...', () => greet(...))` that
        // imports + calls a changed source symbol must land in tier_1. This
        // exercises the parser's test-runner symbol extraction → CALLS edge →
        // reverse traversal in affected_tests.
        use crate::index::index_directory_in_memory;
        use std::fs;

        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(
            src.join("util.ts"),
            "export function greet(name: string): string {\n  return name;\n}\n",
        )
        .expect("write util");
        fs::write(
            src.join("app.test.ts"),
            "import { greet } from './util';\ntest('greets', () => {\n  expect(greet('x')).toBe('x');\n});\n",
        )
        .expect("write test");

        let (_result, store) =
            index_directory_in_memory(dir.path(), "test", "https://example.com/repo", "abc123")
                .expect("index");

        let result = affected_tests(&store, &["src/util.ts".to_string()]).expect("affected");

        let t1_files: Vec<&str> = result.tier_1.iter().map(|f| f.test_file.as_str()).collect();
        assert!(
            t1_files.iter().any(|f| f.ends_with("app.test.ts")),
            "app.test.ts should be in tier_1; tiers: t1={:?} t2={:?} t3={:?}, summary={}",
            t1_files,
            result
                .tier_2
                .iter()
                .map(|f| &f.test_file)
                .collect::<Vec<_>>(),
            result
                .tier_3
                .iter()
                .map(|f| &f.test_file)
                .collect::<Vec<_>>(),
            result.summary
        );
    }

    #[test]
    fn duplicate_changed_files_do_not_duplicate_changed_symbols() {
        let store = GraphStore::in_memory().expect("store");
        let changed = sym("sym:changed", "computeTotal", "src/billing.rs");
        store.insert_symbol(&changed).expect("insert");

        let result = affected_tests(
            &store,
            &["src/billing.rs".to_string(), "src/billing.rs".to_string()],
        )
        .expect("ok");

        assert_eq!(
            result
                .changed_symbols
                .iter()
                .filter(|c| c.uid == "sym:changed")
                .count(),
            1,
            "changed symbol must be deduplicated across duplicate changed_files entries"
        );
        assert_eq!(result.status, AnalysisStatus::Complete);
    }

    #[test]
    fn editing_a_test_file_makes_it_tier_1() {
        let store = GraphStore::in_memory().expect("store");
        let test_sym = sym("sym:t", "checks_login", "src/auth.test.ts");
        store.insert_symbol(&test_sym).expect("insert");

        let result = affected_tests(&store, &["src/auth.test.ts".to_string()]).expect("ok");
        assert_eq!(result.tier_1.len(), 1);
        assert_eq!(result.tier_1[0].test_file, "src/auth.test.ts");
        assert!(result.tier_1[0].tests.contains(&"checks_login".to_string()));
    }

    /// nw-064: TIA/Develocity always-include rule — a changed TEST file from
    /// the diff itself is selected even when the index doesn't know it yet
    /// (new test + stale index was a silent miss).
    #[test]
    fn unindexed_changed_test_file_is_always_included() {
        let store = GraphStore::in_memory().expect("store");
        let result = affected_tests(
            &store,
            &["src/new.test.ts".to_string(), "src/also_new.rs".to_string()],
        )
        .expect("ok");
        let t1: Vec<&str> = result.tier_1.iter().map(|f| f.test_file.as_str()).collect();
        assert!(
            t1.contains(&"src/new.test.ts"),
            "changed test file must be tier-1 even when unindexed: {t1:?}"
        );
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "always-include-changed-test"),
            "inclusion must be disclosed: {:?}",
            result.notifications
        );
        // The non-test source file is disclosed as unassessed, not silent.
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "changed-file-no-symbols"
                    && n.message.contains("src/also_new.rs")),
            "unindexed changed source must be disclosed: {:?}",
            result.notifications
        );
    }

    /// The always-include rule must not duplicate a test file the graph
    /// already selected via the edited-test tier-1 rule.
    #[test]
    fn always_include_does_not_duplicate_indexed_selection() {
        let store = GraphStore::in_memory().expect("store");
        let t = sym("sym:t", "checks_login", "src/auth.test.ts");
        store.insert_symbol(&t).expect("insert");
        let result = affected_tests(&store, &["src/auth.test.ts".to_string()]).expect("ok");
        let count = result
            .tier_1
            .iter()
            .filter(|f| f.test_file == "src/auth.test.ts")
            .count();
        assert_eq!(
            count, 1,
            "one entry, not graph + always-include: {:?}",
            result.tier_1
        );
        assert!(
            result.tier_1[0].tests.contains(&"checks_login".to_string()),
            "the indexed entry (with test names) must win"
        );
    }

    #[test]
    fn degraded_run_recommends_full_suite() {
        // Complete run: selection usable end-to-end.
        let store = GraphStore::in_memory().expect("store");
        let ok = affected_tests(&store, &["src/a.rs".to_string()]).expect("ok");
        assert_eq!(ok.status, AnalysisStatus::Complete);
        assert_eq!(ok.recommendation, "selection-usable");
        // Fail-safe widening (TIA precedent): ANY non-complete status must
        // recommend the full suite.
        assert_eq!(
            derive_recommendation(AnalysisStatus::Complete),
            "selection-usable"
        );
        assert_eq!(
            derive_recommendation(AnalysisStatus::Partial),
            "run-full-suite"
        );
        assert_eq!(
            derive_recommendation(AnalysisStatus::Degraded),
            "run-full-suite"
        );
        assert_eq!(
            derive_recommendation(AnalysisStatus::Failed),
            "run-full-suite"
        );
    }
}

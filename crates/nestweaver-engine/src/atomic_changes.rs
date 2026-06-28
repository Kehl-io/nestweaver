//! Chianti-style atomic change diffing for symbol diffs.
//!
//! Given two sets of parsed symbols (old and new) for a single file, produces
//! a list of typed atomic changes. Adapted from Chianti (Ren et al., OOPSLA
//! 2004) with a language-agnostic subset of 7 change types.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nestweaver_parser::RawSymbol;
use nestweaver_schema::{SymbolKind, Visibility, canonical_symbol_id};
use serde::{Deserialize, Serialize};

/// A single typed change to a symbol.
///
/// Language-agnostic subset of Chianti's 16 Java-specific types:
///
/// | Type              | Trigger                                     |
/// |-------------------|---------------------------------------------|
/// | SymbolAdded       | New symbol not in old                       |
/// | SymbolRemoved     | Old symbol not in new                       |
/// | SignatureChanged  | Same symbol, different signature             |
/// | SymbolRenamed     | Symbol removed + added with similar sig      |
/// | SymbolMoved       | Same symbol appears in different file        |
/// | ExportAdded       | Symbol gains pub/export visibility           |
/// | ExportRemoved     | Symbol loses pub/export visibility           |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AtomicChange {
    SymbolAdded {
        name: String,
        kind: SymbolKind,
        signature: String,
        file_path: String,
    },
    SymbolRemoved {
        canonical_id: String,
        name: String,
        kind: SymbolKind,
        file_path: String,
    },
    SignatureChanged {
        canonical_id: String,
        name: String,
        old_signature: String,
        new_signature: String,
        file_path: String,
    },
    SymbolRenamed {
        old_canonical_id: String,
        old_name: String,
        new_name: String,
        new_canonical_id: String,
        file_path: String,
    },
    SymbolMoved {
        canonical_id: String,
        name: String,
        old_file: String,
        new_file: String,
    },
    ExportAdded {
        canonical_id: String,
        name: String,
        file_path: String,
    },
    ExportRemoved {
        canonical_id: String,
        name: String,
        file_path: String,
    },
}

impl AtomicChange {
    /// The canonical_id of the changed symbol, for server-side lookup.
    pub fn canonical_id(&self) -> Option<&str> {
        match self {
            AtomicChange::SymbolAdded { .. } => None,
            AtomicChange::SymbolRemoved { canonical_id, .. }
            | AtomicChange::SignatureChanged { canonical_id, .. }
            | AtomicChange::SymbolMoved { canonical_id, .. }
            | AtomicChange::ExportAdded { canonical_id, .. }
            | AtomicChange::ExportRemoved { canonical_id, .. } => Some(canonical_id),
            AtomicChange::SymbolRenamed {
                old_canonical_id, ..
            } => Some(old_canonical_id),
        }
    }
}

/// Compute a canonical ID for a `RawSymbol` within a given file and repo.
fn raw_symbol_canonical_id(sym: &RawSymbol, file_path: &str, repo_url: &str) -> String {
    let scope = sym.scope_chain.as_deref().unwrap_or("");
    canonical_symbol_id(repo_url, file_path, &sym.name, scope)
}

/// Match old and new symbols by canonical_id to detect changes.
///
/// Matching strategy:
/// 1. Match by canonical_id (scope-based, stable across line shifts)
/// 2. Unmatched old symbols: look for a rename (same kind + similar signature)
/// 3. Remaining unmatched old: SymbolRemoved
/// 4. Remaining unmatched new: SymbolAdded
/// 5. Matched pairs with different signatures: SignatureChanged
/// 6. Matched pairs with visibility change: ExportAdded / ExportRemoved
pub fn diff_symbols(
    old_symbols: &[RawSymbol],
    new_symbols: &[RawSymbol],
    file_path: &str,
    repo_url: &str,
) -> Vec<AtomicChange> {
    let mut changes = Vec::new();

    // Build canonical_id -> symbol maps
    let old_map: HashMap<String, &RawSymbol> = old_symbols
        .iter()
        .map(|s| (raw_symbol_canonical_id(s, file_path, repo_url), s))
        .collect();

    let new_map: HashMap<String, &RawSymbol> = new_symbols
        .iter()
        .map(|s| (raw_symbol_canonical_id(s, file_path, repo_url), s))
        .collect();

    let mut matched_old: HashSet<String> = HashSet::new();
    let mut matched_new: HashSet<String> = HashSet::new();

    // Phase 1: Match by canonical_id
    for (cid, old_sym) in &old_map {
        if let Some(new_sym) = new_map.get(cid) {
            matched_old.insert(cid.clone());
            matched_new.insert(cid.clone());

            // Check signature change
            if old_sym.signature != new_sym.signature {
                changes.push(AtomicChange::SignatureChanged {
                    canonical_id: cid.clone(),
                    name: old_sym.name.clone(),
                    old_signature: old_sym.signature.clone(),
                    new_signature: new_sym.signature.clone(),
                    file_path: file_path.to_string(),
                });
            }

            // Check visibility change (export added/removed)
            let was_public = matches!(old_sym.visibility, Visibility::Public);
            let is_public = matches!(new_sym.visibility, Visibility::Public);
            if !was_public && is_public {
                changes.push(AtomicChange::ExportAdded {
                    canonical_id: cid.clone(),
                    name: new_sym.name.clone(),
                    file_path: file_path.to_string(),
                });
            } else if was_public && !is_public {
                changes.push(AtomicChange::ExportRemoved {
                    canonical_id: cid.clone(),
                    name: old_sym.name.clone(),
                    file_path: file_path.to_string(),
                });
            }
        }
    }

    // Phase 2: Detect renames among unmatched symbols
    let unmatched_old: Vec<(&String, &&RawSymbol)> = old_map
        .iter()
        .filter(|(cid, _)| !matched_old.contains(*cid))
        .collect();
    let mut unmatched_new: Vec<(&String, &&RawSymbol)> = new_map
        .iter()
        .filter(|(cid, _)| !matched_new.contains(*cid))
        .collect();

    for (old_cid, old_sym) in &unmatched_old {
        // Look for a rename: same kind, similar signature, different name
        if let Some(pos) = unmatched_new.iter().position(|(_, new_sym)| {
            new_sym.kind == old_sym.kind
                && new_sym.name != old_sym.name
                && signatures_similar(&old_sym.signature, &new_sym.signature)
        }) {
            let (new_cid, _new_sym) = unmatched_new.remove(pos);
            matched_old.insert((*old_cid).clone());
            matched_new.insert((*new_cid).clone());
            changes.push(AtomicChange::SymbolRenamed {
                old_canonical_id: (*old_cid).clone(),
                old_name: old_sym.name.clone(),
                new_name: _new_sym.name.clone(),
                new_canonical_id: (*new_cid).clone(),
                file_path: file_path.to_string(),
            });
        }
    }

    // Phase 3: Remaining unmatched old = removed, unmatched new = added
    for (cid, sym) in &old_map {
        if !matched_old.contains(cid) {
            changes.push(AtomicChange::SymbolRemoved {
                canonical_id: cid.clone(),
                name: sym.name.clone(),
                kind: sym.kind,
                file_path: file_path.to_string(),
            });
        }
    }

    for (cid, sym) in &new_map {
        if !matched_new.contains(cid) {
            changes.push(AtomicChange::SymbolAdded {
                name: sym.name.clone(),
                kind: sym.kind,
                signature: sym.signature.clone(),
                file_path: file_path.to_string(),
            });
        }
    }

    changes
}

/// Heuristic: two signatures are "similar" if they share the same parameter
/// count. Used for rename detection.
fn signatures_similar(a: &str, b: &str) -> bool {
    count_params(a) == count_params(b)
}

/// Count the number of parameters in a function signature string.
fn count_params(sig: &str) -> usize {
    if let Some(start) = sig.find('(') {
        if let Some(end) = sig[start..].find(')') {
            let params = &sig[start + 1..start + end];
            let trimmed = params.trim();
            if trimmed.is_empty() {
                return 0;
            }
            // Filter out self/&self/cls -- not real parameters
            return trimmed
                .split(',')
                .filter(|p| {
                    let p = p.trim();
                    !p.starts_with("self")
                        && !p.starts_with("&self")
                        && !p.starts_with("&mut self")
                        && !p.starts_with("cls")
                })
                .count();
        }
    }
    0
}

/// Compute atomic changes for a single file by parsing old and new content.
///
/// `old_content`: the file content at the last indexed commit
/// `new_content`: the current working tree content
/// `file_path`: repo-relative path
/// `repo_url`: the repo URL (for canonical_id computation)
pub fn compute_file_changes(
    old_content: &str,
    new_content: &str,
    file_path: &str,
    repo_url: &str,
) -> Result<Vec<AtomicChange>, anyhow::Error> {
    let path = Path::new(file_path);
    let old_result = nestweaver_parser::parse_source(path, old_content)?;
    let new_result = nestweaver_parser::parse_source(path, new_content)?;
    Ok(diff_symbols(
        &old_result.symbols,
        &new_result.symbols,
        file_path,
        repo_url,
    ))
}

/// Compute all atomic changes in the working tree vs the last indexed state.
///
/// 1. Run `git diff --name-only HEAD` to find changed files
/// 2. For each changed file that's a supported language:
///    a. Read the old content from `git show HEAD:<file>`
///    b. Read the new content from the working tree
///    c. Diff symbols
/// 3. Return the combined list of atomic changes
pub fn compute_local_changes(
    repo_path: &Path,
    repo_url: &str,
) -> Result<Vec<AtomicChange>, anyhow::Error> {
    use anyhow::Context;
    use std::process::Command;

    // Unstaged changes
    let output = Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-only HEAD")?;

    let changed_files: Vec<&str> = std::str::from_utf8(&output.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    // Staged but not yet committed changes
    let staged = Command::new("git")
        .args(["diff", "--name-only", "--cached"])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-only --cached")?;

    let staged_files: Vec<&str> = std::str::from_utf8(&staged.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    let mut all_files: HashSet<String> = HashSet::new();
    for f in changed_files.iter().chain(staged_files.iter()) {
        all_files.insert(f.to_string());
    }

    let mut all_changes = Vec::new();

    for file in &all_files {
        let path = Path::new(file.as_str());
        // Skip non-code files
        if nestweaver_parser::detect_language(path).is_none() {
            continue;
        }

        // Get old content from HEAD
        let old_output = Command::new("git")
            .args(["show", &format!("HEAD:{}", file)])
            .current_dir(repo_path)
            .output();

        let old_content = match old_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(), // New file -- no old content
        };

        // Get new content from working tree
        let new_content = match std::fs::read_to_string(repo_path.join(file)) {
            Ok(c) => c,
            Err(_) => String::new(), // Deleted file -- no new content
        };

        if old_content.is_empty() && new_content.is_empty() {
            continue;
        }

        match compute_file_changes(&old_content, &new_content, file, repo_url) {
            Ok(changes) => all_changes.extend(changes),
            Err(e) => {
                tracing::warn!(file, error = %e, "failed to diff file, skipping");
            }
        }
    }

    Ok(all_changes)
}

// ── Compatibility classification (Task 8) ────────────────────────────

/// Severity classification for impact analysis results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImpactSeverity {
    Breaking,
    Warning,
    Info,
}

impl std::fmt::Display for ImpactSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImpactSeverity::Breaking => write!(f, "BREAKING"),
            ImpactSeverity::Warning => write!(f, "WARNING"),
            ImpactSeverity::Info => write!(f, "INFO"),
        }
    }
}

/// A single impact result from server-side analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactResult {
    pub change_canonical_id: String,
    pub change_kind: String,
    pub affected_canonical_id: String,
    pub affected_name: String,
    pub affected_repo_url: String,
    pub affected_file: String,
    pub affected_line: u32,
    pub affected_signature: String,
    pub severity: ImpactSeverity,
    pub reason: String,
}

/// Classify the base severity of an atomic change (before considering callers).
pub fn classify_change(change: &AtomicChange) -> ImpactSeverity {
    match change {
        AtomicChange::SymbolRemoved { .. } => ImpactSeverity::Breaking,
        AtomicChange::ExportRemoved { .. } => ImpactSeverity::Breaking,
        AtomicChange::SignatureChanged { .. } => ImpactSeverity::Breaking,
        AtomicChange::SymbolRenamed { .. } => ImpactSeverity::Breaking,
        AtomicChange::SymbolMoved { .. } => ImpactSeverity::Warning,
        AtomicChange::SymbolAdded { .. } => ImpactSeverity::Info,
        AtomicChange::ExportAdded { .. } => ImpactSeverity::Info,
    }
}

/// Classify a signature change by comparing old and new signatures.
/// Takes the affected file path to determine static vs dynamic language.
pub fn classify_signature_change(
    old_sig: &str,
    new_sig: &str,
    affected_file_path: &str,
) -> ImpactSeverity {
    let old_params = count_params(old_sig);
    let new_params = count_params(new_sig);
    let is_dynamic = is_dynamic_language_file(affected_file_path);

    if new_params > old_params {
        if has_default_params(new_sig, old_params) {
            ImpactSeverity::Info
        } else if is_dynamic {
            ImpactSeverity::Warning
        } else {
            ImpactSeverity::Breaking
        }
    } else if new_params < old_params {
        if is_dynamic {
            ImpactSeverity::Warning
        } else {
            ImpactSeverity::Breaking
        }
    } else {
        // Same param count — could be type change or return type change
        ImpactSeverity::Warning
    }
}

fn is_dynamic_language_file(path: &str) -> bool {
    path.ends_with(".py")
        || path.ends_with(".js")
        || path.ends_with(".rb")
        || path.ends_with(".lua")
        || path.ends_with(".php")
}

fn has_default_params(sig: &str, old_count: usize) -> bool {
    if let Some(start) = sig.find('(') {
        if let Some(end) = sig[start..].find(')') {
            let params_str = &sig[start + 1..start + end];
            let params: Vec<&str> = params_str
                .split(',')
                .filter(|p| {
                    let p = p.trim();
                    !p.starts_with("self")
                        && !p.starts_with("&self")
                        && !p.starts_with("&mut self")
                        && !p.starts_with("cls")
                })
                .collect();
            if params.len() > old_count {
                let new_params = &params[old_count..];
                return new_params
                    .iter()
                    .all(|p| p.contains('=') || p.contains("Option<"));
            }
        }
    }
    false
}

/// Format a human-readable reason string for an impact result.
pub fn format_impact_reason(change: &AtomicChange, severity: &ImpactSeverity) -> String {
    match change {
        AtomicChange::SignatureChanged {
            name,
            old_signature,
            new_signature,
            ..
        } => {
            let old_count = count_params(old_signature);
            let new_count = count_params(new_signature);
            match severity {
                ImpactSeverity::Breaking => {
                    if new_count > old_count {
                        format!(
                            "{}(): parameter count changed ({} -> {}) — call site passes {} args",
                            name, old_count, new_count, old_count
                        )
                    } else if new_count < old_count {
                        format!(
                            "{}(): parameter removed — call site passes {} args but function now takes {}",
                            name, old_count, new_count
                        )
                    } else {
                        format!(
                            "{}(): signature changed — {} -> {}",
                            name, old_signature, new_signature
                        )
                    }
                }
                ImpactSeverity::Warning => {
                    format!(
                        "{}(): signature changed (dynamic language) — {} -> {}",
                        name, old_signature, new_signature
                    )
                }
                ImpactSeverity::Info => {
                    format!(
                        "{}(): new parameters have defaults — existing call sites likely unaffected",
                        name
                    )
                }
            }
        }
        AtomicChange::SymbolRemoved { name, .. } => {
            format!("'{}' was removed — reference will break", name)
        }
        AtomicChange::ExportRemoved { name, .. } => {
            format!("'{}' export was removed — import will fail", name)
        }
        AtomicChange::SymbolRenamed {
            old_name, new_name, ..
        } => {
            format!(
                "renamed from '{}' to '{}' — import will fail",
                old_name, new_name
            )
        }
        AtomicChange::SymbolMoved {
            name,
            old_file,
            new_file,
            ..
        } => {
            format!(
                "'{}' moved from {} to {} — import path will break",
                name, old_file, new_file
            )
        }
        _ => String::new(),
    }
}

/// Check if a file path looks like a test file.
pub fn is_test_file(path: &str) -> bool {
    path.contains("/test/")
        || path.contains("/tests/")
        || path.contains("_test.")
        || path.contains(".test.")
        || path.contains(".spec.")
        || path.ends_with("_test.rs")
        || path.ends_with("_test.go")
        || path.ends_with("_test.py")
}

// ── Server-side impact analysis (Task 7) ─────────────────────────────

/// Server-side impact analysis: given atomic changes, query the graph store
/// for affected symbols and classify severity.
///
/// For each change:
/// - SignatureChanged -> find all callers via depth-bounded traversal, classify
/// - SymbolRemoved / ExportRemoved -> find all references, mark as BREAKING
/// - SymbolRenamed -> find all importers, mark as BREAKING
/// - SymbolMoved -> find all importers, mark as WARNING
/// - SymbolAdded / ExportAdded -> no impact (no existing dependents)
pub fn analyze_impact(
    store: &nestweaver_store::GraphStore,
    changes: &[AtomicChange],
    max_depth: u32,
    include_tests: bool,
) -> Result<Vec<ImpactResult>, anyhow::Error> {
    let mut impacts = Vec::new();

    // Cache repo_uid -> repo_url mappings
    let repos = store.list_repos(None)?;
    let repo_url_map: HashMap<String, String> = repos
        .into_iter()
        .map(|r| (r.uid.clone(), r.url.clone()))
        .collect();

    let resolve_repo_url = |repo_uid: &str| -> String {
        repo_url_map
            .get(repo_uid)
            .cloned()
            .unwrap_or_else(|| repo_uid.to_string())
    };

    // Depth-bounded traversal: collect direct references (depth 1) then
    // transitively follow callers up to max_depth.  Direct references get
    // the natural severity; each additional hop downgrades to Warning/Info
    // because the call site is indirectly affected.
    let effective_depth = max_depth.max(1);

    for change in changes {
        match change {
            AtomicChange::SignatureChanged {
                canonical_id,
                name: _,
                old_signature,
                new_signature,
                file_path: _,
            } => {
                if let Some(symbol) = store.symbol_by_canonical_id(canonical_id)? {
                    let direct_severity =
                        classify_signature_change(old_signature, new_signature, &symbol.file_path);
                    let direct_reason = format_impact_reason(change, &direct_severity);
                    collect_transitive_references(
                        store,
                        &symbol.uid,
                        canonical_id,
                        "SIGNATURE_CHANGED",
                        direct_severity,
                        &direct_reason,
                        effective_depth,
                        include_tests,
                        &resolve_repo_url,
                        &mut impacts,
                    );
                }
            }
            AtomicChange::SymbolRemoved {
                canonical_id, name, ..
            }
            | AtomicChange::ExportRemoved {
                canonical_id, name, ..
            } => {
                let change_kind = if matches!(change, AtomicChange::ExportRemoved { .. }) {
                    "EXPORT_REMOVED"
                } else {
                    "SYMBOL_REMOVED"
                };
                if let Some(symbol) = store.symbol_by_canonical_id(canonical_id)? {
                    let reason = format!("'{}' was removed — reference will break", name);
                    collect_transitive_references(
                        store,
                        &symbol.uid,
                        canonical_id,
                        change_kind,
                        ImpactSeverity::Breaking,
                        &reason,
                        effective_depth,
                        include_tests,
                        &resolve_repo_url,
                        &mut impacts,
                    );
                }
            }
            AtomicChange::SymbolRenamed {
                old_canonical_id,
                old_name,
                new_name,
                ..
            } => {
                if let Some(symbol) = store.symbol_by_canonical_id(old_canonical_id)? {
                    let importers = store.importers_of(&symbol.uid)?;
                    for importer in importers {
                        if !include_tests && is_test_file(&importer.file_path) {
                            continue;
                        }
                        let repo_url = resolve_repo_url(&importer.repo_uid);
                        impacts.push(ImpactResult {
                            change_canonical_id: old_canonical_id.clone(),
                            change_kind: "SYMBOL_RENAMED".to_string(),
                            affected_canonical_id: importer
                                .canonical_id
                                .clone()
                                .unwrap_or_default(),
                            affected_name: importer.name.clone(),
                            affected_repo_url: repo_url,
                            affected_file: importer.file_path.clone(),
                            affected_line: importer.start_line,
                            affected_signature: importer.signature.clone(),
                            severity: ImpactSeverity::Breaking,
                            reason: format!(
                                "renamed from '{}' to '{}' — import will fail",
                                old_name, new_name
                            ),
                        });
                    }
                }
            }
            AtomicChange::SymbolAdded { .. } | AtomicChange::ExportAdded { .. } => {
                // No impact — new symbols have no existing dependents
            }
            AtomicChange::SymbolMoved {
                canonical_id,
                name,
                old_file,
                new_file,
            } => {
                if let Some(symbol) = store.symbol_by_canonical_id(canonical_id)? {
                    let importers = store.importers_of(&symbol.uid)?;
                    for importer in importers {
                        if !include_tests && is_test_file(&importer.file_path) {
                            continue;
                        }
                        let repo_url = resolve_repo_url(&importer.repo_uid);
                        impacts.push(ImpactResult {
                            change_canonical_id: canonical_id.clone(),
                            change_kind: "SYMBOL_MOVED".to_string(),
                            affected_canonical_id: importer
                                .canonical_id
                                .clone()
                                .unwrap_or_default(),
                            affected_name: importer.name.clone(),
                            affected_repo_url: repo_url,
                            affected_file: importer.file_path.clone(),
                            affected_line: importer.start_line,
                            affected_signature: String::new(),
                            severity: ImpactSeverity::Warning,
                            reason: format!(
                                "'{}' moved from {} to {} — import path may break",
                                name, old_file, new_file
                            ),
                        });
                    }
                }
            }
        }
    }

    Ok(impacts)
}

/// Collect direct references to `root_uid` and then transitively follow
/// callers up to `max_depth`. Direct references (depth 1) inherit the
/// provided `direct_severity`; deeper hops are downgraded to at most
/// Warning (depth 2) or Info (depth 3+).
fn collect_transitive_references(
    store: &nestweaver_store::GraphStore,
    root_uid: &str,
    change_canonical_id: &str,
    change_kind: &str,
    direct_severity: ImpactSeverity,
    direct_reason: &str,
    max_depth: u32,
    include_tests: bool,
    resolve_repo_url: &dyn Fn(&str) -> String,
    impacts: &mut Vec<ImpactResult>,
) {
    let mut visited = HashSet::new();
    visited.insert(root_uid.to_string());

    // BFS frontier: (uid, depth)
    let mut frontier: Vec<(String, u32)> = vec![(root_uid.to_string(), 0)];

    while let Some((uid, depth)) = frontier.pop() {
        if depth >= max_depth {
            continue;
        }

        let refs = match store.references_to(&uid) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for ref_sym in refs {
            if visited.contains(&ref_sym.uid) {
                continue;
            }
            visited.insert(ref_sym.uid.clone());

            if !include_tests && is_test_file(&ref_sym.file_path) {
                continue;
            }

            let hop = depth + 1;
            let severity = if hop == 1 {
                direct_severity
            } else if hop == 2 {
                // Indirect caller — at most Warning.
                match direct_severity {
                    ImpactSeverity::Breaking => ImpactSeverity::Warning,
                    other => other,
                }
            } else {
                ImpactSeverity::Info
            };

            let reason = if hop == 1 {
                direct_reason.to_string()
            } else {
                format!(
                    "{} (indirect caller, {} hop{} away)",
                    direct_reason,
                    hop,
                    if hop > 1 { "s" } else { "" }
                )
            };

            let repo_url = resolve_repo_url(&ref_sym.repo_uid);
            impacts.push(ImpactResult {
                change_canonical_id: change_canonical_id.to_string(),
                change_kind: change_kind.to_string(),
                affected_canonical_id: ref_sym.canonical_id.clone().unwrap_or_default(),
                affected_name: ref_sym.name.clone(),
                affected_repo_url: repo_url,
                affected_file: ref_sym.file_path.clone(),
                affected_line: ref_sym.start_line,
                affected_signature: ref_sym.signature.clone(),
                severity,
                reason,
            });

            // Enqueue for deeper traversal
            if hop < max_depth {
                frontier.push((ref_sym.uid.clone(), hop));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_URL: &str = "https://github.com/acme/api";

    /// analyze_impact (the ImpactAnalysis RPC / pre-push-impact path) must report
    /// consumers in OTHER repos. Regression guard for cross-boundary intelligence:
    /// removing a symbol used by a downstream repo via a cross-repo link must be
    /// flagged, not silently dropped at the repo boundary.
    #[test]
    fn analyze_impact_reports_cross_repo_consumers() {
        use nestweaver_schema::{
            CrossRepoLinkType, EdgeType, Repo, ResolvedEdge, Symbol, SymbolKind, Visibility,
        };
        use nestweaver_store::GraphStore;

        let store = GraphStore::in_memory().unwrap();

        // Two repos: an api repo that owns the changed symbol, and a client repo
        // whose symbol consumes it across the boundary.
        for (uid, url) in [
            ("repo:api", "https://github.com/acme/api"),
            ("repo:client", "https://github.com/acme/client"),
        ] {
            store
                .insert_repo(&Repo {
                    uid: uid.to_string(),
                    url: url.to_string(),
                    indexed_sha: "sha".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: "inst".to_string(),
                    name: None,
                })
                .unwrap();
        }

        let canonical =
            canonical_symbol_id("https://github.com/acme/api", "src/api.rs", "Handler", "");
        let mk = |uid: &str, name: &str, repo: &str, file: &str, cid: Option<String>| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo.to_string(),
            file_path: file.to_string(),
            start_line: 5,
            end_line: 9,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: cid,
        };
        store
            .insert_symbol(&mk(
                "api",
                "Handler",
                "repo:api",
                "src/api.rs",
                Some(canonical.clone()),
            ))
            .unwrap();
        store
            .insert_symbol(&mk(
                "client",
                "Caller",
                "repo:client",
                "src/client.rs",
                None,
            ))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "client".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: vec![],
            })
            .unwrap();

        let changes = vec![AtomicChange::SymbolRemoved {
            canonical_id: canonical,
            name: "Handler".to_string(),
            kind: SymbolKind::Function,
            file_path: "src/api.rs".to_string(),
        }];

        let impacts = analyze_impact(&store, &changes, 5, true).unwrap();
        assert!(
            impacts.iter().any(|i| i.affected_name == "Caller"
                && i.affected_repo_url == "https://github.com/acme/client"),
            "analyze_impact must report the cross-repo consumer; got: {:?}",
            impacts
                .iter()
                .map(|i| (&i.affected_name, &i.affected_repo_url))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn detects_signature_change() {
        let old = "pub fn process(amount: f64) -> bool { true }";
        let new = "pub fn process(amount: f64, currency: &str) -> bool { true }";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let sig_change = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SignatureChanged { .. }));
        assert!(sig_change.is_some(), "should detect signature change");
    }

    #[test]
    fn detects_symbol_removed() {
        let old = "pub fn foo() {}\npub fn bar() {}";
        let new = "pub fn foo() {}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let removed = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SymbolRemoved { name, .. } if name == "bar"));
        assert!(removed.is_some(), "should detect bar was removed");
    }

    #[test]
    fn detects_symbol_added() {
        let old = "pub fn foo() {}";
        let new = "pub fn foo() {}\npub fn bar() {}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let added = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SymbolAdded { name, .. } if name == "bar"));
        assert!(added.is_some(), "should detect bar was added");
    }

    #[test]
    fn detects_export_removed() {
        let old = "pub fn foo() {}";
        let new = "fn foo() {}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let export_removed = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::ExportRemoved { .. }));
        assert!(export_removed.is_some(), "should detect export was removed");
    }

    #[test]
    fn detects_export_added() {
        let old = "fn foo() {}";
        let new = "pub fn foo() {}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let export_added = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::ExportAdded { .. }));
        assert!(export_added.is_some(), "should detect export was added");
    }

    #[test]
    fn no_changes_for_identical_files() {
        let content = "pub fn foo() {}";
        let changes = compute_file_changes(content, content, "src/lib.rs", REPO_URL).unwrap();
        assert!(
            changes.is_empty(),
            "identical files should produce no changes"
        );
    }

    #[test]
    fn line_shift_does_not_produce_false_positive_for_scoped_symbols() {
        // Adding a comment above should NOT trigger a change for SCOPED symbols
        // (those with a scope chain like Foo::bar). Canonical IDs are line-shift
        // stable, so neither scoped nor top-level symbols change identity here.
        let old = "impl Foo {\n    pub fn bar(&self) {}\n}";
        let new = "// new comment\nimpl Foo {\n    pub fn bar(&self) {}\n}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        // bar (scoped as Foo::bar) should NOT produce a false positive
        let bar_changes: Vec<_> = changes
            .iter()
            .filter(|c| match c {
                AtomicChange::SymbolAdded { name, .. }
                | AtomicChange::SymbolRemoved { name, .. }
                | AtomicChange::SignatureChanged { name, .. } => name == "bar",
                _ => false,
            })
            .collect();
        assert!(
            bar_changes.is_empty(),
            "scoped symbol 'bar' should not produce false positives on line shift; got: {:?}",
            bar_changes
        );
    }

    #[test]
    fn line_shift_does_not_produce_false_positive_for_top_level_symbols() {
        // Inserting blank lines / a comment above a TOP-LEVEL function (empty
        // scope chain) must not register as a change. Canonical IDs are stable
        // across line shifts, so the symbol keeps its identity. Regression guard
        // for the canonical_id line-shift instability bug.
        let old = "pub fn helper(x: i32) -> i32 { x + 1 }";
        let new = "// added a doc line\n\npub fn helper(x: i32) -> i32 { x + 1 }";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let helper_changes: Vec<_> = changes
            .iter()
            .filter(|c| match c {
                AtomicChange::SymbolAdded { name, .. }
                | AtomicChange::SymbolRemoved { name, .. }
                | AtomicChange::SignatureChanged { name, .. } => name == "helper",
                _ => false,
            })
            .collect();
        assert!(
            helper_changes.is_empty(),
            "top-level symbol 'helper' should not produce false positives on line shift; got: {:?}",
            helper_changes
        );
    }

    #[test]
    fn detects_renamed_symbol() {
        let old = "pub fn process_payment(amount: f64) -> bool { true }";
        let new = "pub fn handle_payment(amount: f64) -> bool { true }";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let renamed = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SymbolRenamed { .. }));
        assert!(
            renamed.is_some(),
            "should detect rename; got: {:?}",
            changes
        );
    }

    #[test]
    fn count_params_works() {
        assert_eq!(count_params("fn foo()"), 0);
        assert_eq!(count_params("fn foo(a: i32)"), 1);
        assert_eq!(count_params("fn foo(a: i32, b: String)"), 2);
        assert_eq!(count_params("fn foo(&self, a: i32)"), 1);
        assert_eq!(count_params("fn foo(self, a: i32)"), 1);
        assert_eq!(count_params("def foo(cls, a)"), 1);
    }

    #[test]
    fn compute_local_changes_detects_working_tree_changes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();

        // Initial commit
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn process(amount: f64) -> bool { true }",
        )
        .unwrap();
        init_git_repo(&repo);
        git_add_commit(&repo, "initial");

        // Modify file (not committed)
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn process(amount: f64, currency: &str) -> bool { true }",
        )
        .unwrap();

        let changes = compute_local_changes(&repo, REPO_URL).unwrap();
        assert!(!changes.is_empty(), "should detect local changes");
        let sig_change = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SignatureChanged { .. }));
        assert!(
            sig_change.is_some(),
            "should detect signature change; got: {:?}",
            changes
        );
    }

    #[test]
    fn compute_local_changes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();

        // Initial commit with one file
        std::fs::write(repo.join("src/lib.rs"), "pub fn foo() {}").unwrap();
        init_git_repo(&repo);
        git_add_commit(&repo, "initial");

        // Add new file (not committed)
        std::fs::write(repo.join("src/bar.rs"), "pub fn bar() {}").unwrap();
        // Stage it so git diff --cached picks it up
        std::process::Command::new("git")
            .args(["add", "src/bar.rs"])
            .current_dir(&repo)
            .output()
            .unwrap();

        let changes = compute_local_changes(&repo, REPO_URL).unwrap();
        let added = changes
            .iter()
            .find(|c| matches!(c, AtomicChange::SymbolAdded { name, .. } if name == "bar"));
        assert!(
            added.is_some(),
            "should detect new file symbols as added; got: {:?}",
            changes
        );
    }

    #[test]
    fn compute_local_changes_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();

        std::fs::write(repo.join("src/lib.rs"), "pub fn foo() {}").unwrap();
        init_git_repo(&repo);
        git_add_commit(&repo, "initial");

        // No modifications
        let changes = compute_local_changes(&repo, REPO_URL).unwrap();
        assert!(
            changes.is_empty(),
            "should detect no changes; got: {:?}",
            changes
        );
    }

    // ── Compatibility classification tests ──

    #[test]
    fn classify_removed_is_breaking() {
        let change = AtomicChange::SymbolRemoved {
            canonical_id: "test:src/lib.rs#foo:abc".into(),
            name: "foo".into(),
            kind: SymbolKind::Function,
            file_path: "src/lib.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Breaking);
    }

    #[test]
    fn classify_export_removed_is_breaking() {
        let change = AtomicChange::ExportRemoved {
            canonical_id: "test:src/lib.rs#foo:abc".into(),
            name: "foo".into(),
            file_path: "src/lib.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Breaking);
    }

    #[test]
    fn classify_added_is_info() {
        let change = AtomicChange::SymbolAdded {
            name: "bar".into(),
            kind: SymbolKind::Function,
            signature: "fn bar()".into(),
            file_path: "src/lib.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Info);
    }

    #[test]
    fn classify_export_added_is_info() {
        let change = AtomicChange::ExportAdded {
            canonical_id: "test:src/lib.rs#foo:abc".into(),
            name: "foo".into(),
            file_path: "src/lib.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Info);
    }

    #[test]
    fn classify_renamed_is_breaking() {
        let change = AtomicChange::SymbolRenamed {
            old_canonical_id: "test:src/lib.rs#foo:abc".into(),
            old_name: "foo".into(),
            new_name: "bar".into(),
            new_canonical_id: "test:src/lib.rs#bar:def".into(),
            file_path: "src/lib.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Breaking);
    }

    #[test]
    fn classify_moved_is_warning() {
        let change = AtomicChange::SymbolMoved {
            canonical_id: "test:src/old.rs#foo:abc".into(),
            name: "foo".into(),
            old_file: "src/old.rs".into(),
            new_file: "src/new.rs".into(),
        };
        assert_eq!(classify_change(&change), ImpactSeverity::Warning);
    }

    #[test]
    fn classify_sig_add_required_param_static_is_breaking() {
        let severity = classify_signature_change(
            "fn foo(a: i32) -> bool",
            "fn foo(a: i32, b: String) -> bool",
            "src/caller.rs",
        );
        assert_eq!(severity, ImpactSeverity::Breaking);
    }

    #[test]
    fn classify_sig_add_optional_param_is_info() {
        let severity =
            classify_signature_change("def foo(a)", "def foo(a, b=None)", "src/caller.py");
        assert_eq!(severity, ImpactSeverity::Info);
    }

    #[test]
    fn classify_sig_add_param_dynamic_is_warning() {
        let severity = classify_signature_change("def foo(a)", "def foo(a, b)", "src/caller.py");
        assert_eq!(severity, ImpactSeverity::Warning);
    }

    #[test]
    fn classify_sig_remove_param_static_is_breaking() {
        let severity = classify_signature_change(
            "fn foo(a: i32, b: String) -> bool",
            "fn foo(a: i32) -> bool",
            "src/caller.rs",
        );
        assert_eq!(severity, ImpactSeverity::Breaking);
    }

    #[test]
    fn classify_sig_same_count_is_warning() {
        let severity = classify_signature_change(
            "fn foo(a: i32) -> bool",
            "fn foo(a: String) -> bool",
            "src/caller.rs",
        );
        assert_eq!(severity, ImpactSeverity::Warning);
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file("src/tests/foo.rs"));
        assert!(is_test_file("src/foo_test.rs"));
        assert!(is_test_file("src/foo.test.ts"));
        assert!(is_test_file("src/foo.spec.js"));
        assert!(!is_test_file("src/foo.rs"));
        assert!(!is_test_file("src/main.ts"));
    }

    // --- Test helpers ---

    fn init_git_repo(path: &Path) {
        use std::process::Command;
        Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    fn git_add_commit(path: &Path, msg: &str) {
        use std::process::Command;
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(path)
            .output()
            .unwrap();
    }
}

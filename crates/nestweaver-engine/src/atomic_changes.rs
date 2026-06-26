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

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_URL: &str = "https://github.com/acme/api";

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
        let removed = changes.iter().find(
            |c| matches!(c, AtomicChange::SymbolRemoved { name, .. } if name == "bar"),
        );
        assert!(removed.is_some(), "should detect bar was removed");
    }

    #[test]
    fn detects_symbol_added() {
        let old = "pub fn foo() {}";
        let new = "pub fn foo() {}\npub fn bar() {}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        let added = changes.iter().find(
            |c| matches!(c, AtomicChange::SymbolAdded { name, .. } if name == "bar"),
        );
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
        assert!(
            export_removed.is_some(),
            "should detect export was removed"
        );
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
    fn line_shift_does_not_produce_false_positive() {
        // Adding a comment above should NOT trigger a change for scoped symbols
        let old = "impl Foo {\n    pub fn bar(&self) {}\n}";
        let new = "// new comment\nimpl Foo {\n    pub fn bar(&self) {}\n}";
        let changes = compute_file_changes(old, new, "src/lib.rs", REPO_URL).unwrap();
        // bar has a scope chain (Foo::bar), so line shift shouldn't matter
        assert!(
            changes.is_empty(),
            "line shift should not produce false positives for scoped symbols; got: {:?}",
            changes
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
        assert!(renamed.is_some(), "should detect rename; got: {:?}", changes);
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

}

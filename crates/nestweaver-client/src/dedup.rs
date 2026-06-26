//! Scope-hash based deduplication for results from local and server sources.
//!
//! Identity is `(repo_url, file_path, symbol_name, scope_hash)`. The scope_hash
//! uses the symbol's scope chain (e.g. `module::class::method`) rather than line
//! number — line numbers shift when code changes above a symbol, but scope chains
//! are stable across versions.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Identity key for deduplicating results across local and server.
/// Uses scope_hash instead of line number for stability across versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolIdentity {
    pub repo_url: String,
    pub file_path: String,
    pub symbol_name: String,
    pub scope_hash: u64,
}

/// Provenance tracking for a merged result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Local,
    Server,
    Both,
}

/// A result with provenance and confidence metadata attached.
#[derive(Debug, Clone)]
pub struct MergedResult {
    pub value: serde_json::Value,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub score: f64,
}

/// Confidence level for a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Symbol resolved by tree-sitter parser with full scope/import resolution
    Precise,
    /// Matched by name/BM25 but not structurally resolved
    Heuristic,
    /// Precise at index time, but file has been modified locally since
    Stale,
}

/// Compute a scope hash from a scope chain like "module::class::method".
/// Falls back to start_line when scope information is unavailable.
pub fn compute_scope_hash(scope_chain: Option<&str>, start_line: Option<u32>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match scope_chain {
        Some(chain) if !chain.is_empty() => chain.hash(&mut hasher),
        _ => {
            // Fallback: use start_line (less stable but better than nothing)
            if let Some(line) = start_line {
                line.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Extract a [`SymbolIdentity`] from a JSON result value.
/// Returns `None` if required fields are missing.
pub fn extract_identity(result: &serde_json::Value) -> Option<SymbolIdentity> {
    let repo_url = result
        .get("repo_url")
        .or_else(|| result.get("repo"))
        .and_then(|v| v.as_str())?
        .to_string();
    let file_path = result
        .get("file_path")
        .or_else(|| result.get("file"))
        .and_then(|v| v.as_str())?
        .to_string();
    let symbol_name = result
        .get("symbol_name")
        .or_else(|| result.get("name"))
        .or_else(|| result.get("symbol"))
        .and_then(|v| v.as_str())?
        .to_string();
    let scope_chain = result
        .get("scope_chain")
        .or_else(|| result.get("scope"))
        .and_then(|v| v.as_str());
    let start_line = result
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let scope_hash = compute_scope_hash(scope_chain, start_line);

    Some(SymbolIdentity {
        repo_url,
        file_path,
        symbol_name,
        scope_hash,
    })
}

/// Deduplicate results from local and server sources.
/// Local wins on content when both have the same symbol.
pub fn deduplicate(
    local_results: Vec<serde_json::Value>,
    server_results: Vec<serde_json::Value>,
) -> Vec<MergedResult> {
    let mut seen: HashMap<SymbolIdentity, MergedResult> = HashMap::new();
    let mut unkeyed: Vec<MergedResult> = Vec::new();

    // Insert local results first (they win on content)
    for (rank, val) in local_results.into_iter().enumerate() {
        let result = MergedResult {
            value: val.clone(),
            provenance: Provenance::Local,
            confidence: Confidence::Precise,
            score: 1.0 / (rank as f64 + 61.0), // RRF with k=60
        };
        match extract_identity(&val) {
            Some(id) => {
                seen.insert(id, result);
            }
            None => {
                unkeyed.push(result);
            }
        }
    }

    // Insert server results, merging with existing local entries
    for (rank, val) in server_results.into_iter().enumerate() {
        match extract_identity(&val) {
            Some(id) => {
                if let Some(existing) = seen.get_mut(&id) {
                    // Both have it — local wins on content, mark as Both
                    existing.provenance = Provenance::Both;
                    // Add server's RRF score
                    existing.score += 1.0 / (rank as f64 + 61.0);
                } else {
                    // Server-only
                    seen.insert(
                        id,
                        MergedResult {
                            value: val,
                            provenance: Provenance::Server,
                            confidence: Confidence::Precise,
                            score: 1.0 / (rank as f64 + 61.0),
                        },
                    );
                }
            }
            None => {
                // No identity — can't dedup, keep server result
                unkeyed.push(MergedResult {
                    value: val,
                    provenance: Provenance::Server,
                    confidence: Confidence::Heuristic,
                    score: 1.0 / (rank as f64 + 61.0),
                });
            }
        }
    }

    let mut results: Vec<_> = seen.into_values().chain(unkeyed).collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scope_hash_stable_for_same_chain() {
        let h1 = compute_scope_hash(Some("module::Class::method"), None);
        let h2 = compute_scope_hash(Some("module::Class::method"), None);
        assert_eq!(h1, h2);
    }

    #[test]
    fn scope_hash_differs_for_different_chains() {
        let h1 = compute_scope_hash(Some("module::ClassA::method"), None);
        let h2 = compute_scope_hash(Some("module::ClassB::method"), None);
        assert_ne!(h1, h2);
    }

    #[test]
    fn scope_hash_falls_back_to_line() {
        let h1 = compute_scope_hash(None, Some(42));
        let h2 = compute_scope_hash(None, Some(42));
        assert_eq!(h1, h2);
        let h3 = compute_scope_hash(None, Some(45));
        assert_ne!(h1, h3);
    }

    #[test]
    fn scope_hash_prefers_chain_over_line() {
        // When scope_chain is present, start_line is ignored
        let h1 = compute_scope_hash(Some("module::func"), Some(10));
        let h2 = compute_scope_hash(Some("module::func"), Some(20));
        assert_eq!(h1, h2);
    }

    #[test]
    fn extract_identity_from_standard_fields() {
        let result = json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "process_payment",
            "scope_chain": "api::billing",
        });
        let id = extract_identity(&result).unwrap();
        assert_eq!(id.repo_url, "github.com/acme/api");
        assert_eq!(id.file_path, "src/lib.rs");
        assert_eq!(id.symbol_name, "process_payment");
    }

    #[test]
    fn extract_identity_from_alternate_fields() {
        let result = json!({
            "repo": "github.com/acme/api",
            "file": "src/lib.rs",
            "name": "process_payment",
            "scope": "api::billing",
        });
        let id = extract_identity(&result).unwrap();
        assert_eq!(id.repo_url, "github.com/acme/api");
        assert_eq!(id.symbol_name, "process_payment");
    }

    #[test]
    fn extract_identity_returns_none_when_missing_fields() {
        let result = json!({ "repo_url": "github.com/acme/api" });
        assert!(extract_identity(&result).is_none());
    }

    #[test]
    fn dedup_local_wins_content() {
        let local = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "process_payment",
            "scope_chain": "api::billing",
            "body": "local version"
        })];
        let server = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "process_payment",
            "scope_chain": "api::billing",
            "body": "server version"
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Both);
        assert_eq!(merged[0].value["body"], "local version"); // local wins
    }

    #[test]
    fn dedup_keeps_server_only_results() {
        let local = vec![];
        let server = vec![json!({
            "repo_url": "github.com/acme/billing",
            "file_path": "src/webhook.rs",
            "symbol_name": "handle_webhook",
            "scope_chain": "billing::webhook",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Server);
    }

    #[test]
    fn dedup_preserves_both_when_different_symbols() {
        let local = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "func_a",
            "scope_chain": "api",
        })];
        let server = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "func_b",
            "scope_chain": "api",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn dedup_combined_score_higher_than_single() {
        let local = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "shared_fn",
            "scope_chain": "api::shared",
        })];
        let server = vec![json!({
            "repo_url": "github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "shared_fn",
            "scope_chain": "api::shared",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 1);
        // Combined score should be higher than a single source
        let single_score = 1.0 / 61.0;
        assert!(merged[0].score > single_score);
    }

    #[test]
    fn dedup_both_empty() {
        let merged = deduplicate(vec![], vec![]);
        assert!(merged.is_empty());
    }

    #[test]
    fn dedup_handles_unkeyed_results() {
        // Results without identity fields are kept as-is
        let local = vec![json!({"text": "some note"})];
        let server = vec![json!({"text": "another note"})];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 2);
    }
}

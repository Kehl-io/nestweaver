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
///
/// Handles both naming conventions:
/// - Standard: `repo_url`/`repo`, `file_path`/`file`, `symbol_name`/`name`/`symbol`
/// - Brain search/context: `uid` (contains repo info), `location` ("file:line"),
///   `title` (symbol name)
///
/// Returns `None` if not enough fields are present to form an identity.
pub fn extract_identity(result: &serde_json::Value) -> Option<SymbolIdentity> {
    // Symbol name: try standard fields first, then brain_search/brain_context `title`.
    let symbol_name = result
        .get("symbol_name")
        .or_else(|| result.get("name"))
        .or_else(|| result.get("symbol"))
        .or_else(|| result.get("title"))
        .and_then(|v| v.as_str())?
        .to_string();

    // File path: try standard fields, then extract from `location` ("path:line").
    let file_path = result
        .get("file_path")
        .or_else(|| result.get("file"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            result.get("location").and_then(|v| v.as_str()).map(|loc| {
                // "src/lib.rs:42" -> "src/lib.rs"
                loc.rsplit_once(':')
                    .map(|(path, _line)| path.to_string())
                    .unwrap_or_else(|| loc.to_string())
            })
        })?;

    // Repo URL: try standard fields, then extract from `uid` if it encodes
    // repo info (e.g. "repo:github.com/acme/api:src/lib.rs#symbol").
    let repo_url = result
        .get("repo_url")
        .or_else(|| result.get("repo"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            result.get("uid").and_then(|v| v.as_str()).and_then(|uid| {
                // UID format: "repo:<url>:<path>#<name>" or "sym:<repo_uid>:<path>#<name>".
                // Extract the repo segment if present.
                uid.split_once(':')
                    .and_then(|(_, rest)| rest.split_once(':'))
                    .map(|(repo_part, _)| repo_part.to_string())
            })
        })
        // If no repo could be extracted, use the file_path as a stand-in so
        // same-file symbols still deduplicate against each other.
        .unwrap_or_else(|| file_path.clone());

    let scope_chain = result
        .get("scope_chain")
        .or_else(|| result.get("scope"))
        .and_then(|v| v.as_str());

    // Start line: try standard field, then parse from `location` ("path:42").
    let start_line = result
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or_else(|| {
            result
                .get("location")
                .and_then(|v| v.as_str())
                .and_then(|loc| loc.rsplit_once(':'))
                .and_then(|(_, line)| line.parse::<u32>().ok())
        });

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

/// Determine confidence for a result based on its provenance and local file state.
///
/// - If the result is from the server and the file has been modified locally,
///   the result is `Stale` (server's index is out of date for that file).
/// - If the result has structural scope information, it is `Precise`.
/// - Otherwise it is `Heuristic`.
pub fn assign_confidence(
    result: &serde_json::Value,
    provenance: Provenance,
    locally_modified_files: &std::collections::HashSet<String>,
) -> Confidence {
    let file_path = result
        .get("file_path")
        .or_else(|| result.get("file"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Server results for locally-modified files are stale.
    if provenance == Provenance::Server && locally_modified_files.contains(file_path) {
        return Confidence::Stale;
    }

    // Check for structural resolution markers.
    let has_scope = result
        .get("scope_chain")
        .or_else(|| result.get("scope"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if has_scope {
        Confidence::Precise
    } else {
        Confidence::Heuristic
    }
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
    fn extract_identity_from_brain_search_fields() {
        // brain_search results use uid, title, location instead of
        // repo_url, symbol_name, file_path.
        let result = json!({
            "uid": "sym:repo:github.com/acme/api:src/lib.rs#process_payment",
            "kind": "Symbol/Function",
            "title": "process_payment",
            "location": "src/lib.rs:42",
        });
        let id = extract_identity(&result).unwrap();
        assert_eq!(id.symbol_name, "process_payment");
        assert_eq!(id.file_path, "src/lib.rs");
    }

    #[test]
    fn extract_identity_from_brain_context_fields() {
        // brain_context connected items use uid, title, location.
        let result = json!({
            "uid": "note:vault:my-vault:notes/design.md",
            "kind": "note",
            "title": "Design Notes",
            "location": "notes/design.md:1",
        });
        let id = extract_identity(&result).unwrap();
        assert_eq!(id.symbol_name, "Design Notes");
        assert_eq!(id.file_path, "notes/design.md");
    }

    #[test]
    fn extract_identity_deduplicates_brain_search_results() {
        // Two results with the same title+location should merge.
        let local = json!({
            "uid": "sym:repo:acme:src/lib.rs#Handler",
            "title": "Handler",
            "location": "src/lib.rs:10",
        });
        let server = json!({
            "uid": "sym:repo:acme:src/lib.rs#Handler",
            "title": "Handler",
            "location": "src/lib.rs:10",
        });
        let id_local = extract_identity(&local).unwrap();
        let id_server = extract_identity(&server).unwrap();
        assert_eq!(id_local, id_server);
    }

    #[test]
    fn extract_identity_returns_none_when_missing_fields() {
        // No title/name/symbol_name at all -> None
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

    // ── assign_confidence tests ──────────────────────────────────

    #[test]
    fn confidence_precise_when_scope_present() {
        let result = json!({ "scope_chain": "module::class::method" });
        let confidence = assign_confidence(
            &result,
            Provenance::Local,
            &std::collections::HashSet::new(),
        );
        assert_eq!(confidence, Confidence::Precise);
    }

    #[test]
    fn confidence_heuristic_when_no_scope() {
        let result = json!({ "symbol_name": "foo" });
        let confidence = assign_confidence(
            &result,
            Provenance::Local,
            &std::collections::HashSet::new(),
        );
        assert_eq!(confidence, Confidence::Heuristic);
    }

    #[test]
    fn confidence_stale_when_server_result_for_modified_file() {
        let result = json!({ "file_path": "src/lib.rs", "scope_chain": "mod::fn" });
        let modified = std::collections::HashSet::from(["src/lib.rs".to_string()]);
        let confidence = assign_confidence(&result, Provenance::Server, &modified);
        assert_eq!(confidence, Confidence::Stale);
    }

    #[test]
    fn confidence_not_stale_for_local_result_of_modified_file() {
        let result = json!({ "file_path": "src/lib.rs", "scope_chain": "mod::fn" });
        let modified = std::collections::HashSet::from(["src/lib.rs".to_string()]);
        // Local results are fresh regardless — stale only applies to server results.
        let confidence = assign_confidence(&result, Provenance::Local, &modified);
        assert_eq!(confidence, Confidence::Precise);
    }

    #[test]
    fn confidence_precise_for_server_result_of_unmodified_file() {
        let result = json!({ "file_path": "src/other.rs", "scope_chain": "mod::fn" });
        let modified = std::collections::HashSet::from(["src/lib.rs".to_string()]);
        let confidence = assign_confidence(&result, Provenance::Server, &modified);
        assert_eq!(confidence, Confidence::Precise);
    }
}

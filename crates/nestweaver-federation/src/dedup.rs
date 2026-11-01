//! Identity-based deduplication for results from local and server sources.
//!
//! A row that carries a uid is keyed by that uid, never by presentation
//! fields such as title or location (nw-898):
//!
//! - A uid the shared schema parser
//!   ([`nestweaver_schema::uid::parse_canonical_entity_uid`]) accepts is keyed
//!   by its domain-qualified, instance-stripped form ([`EntityKey`],
//!   `canonical == true`).
//! - Any other non-empty uid (`proj:`, `contract:`, off-grammar) is keyed by
//!   its raw string (`uid:<raw>`, `canonical == false`). That never drops a
//!   distinct row; at worst it fails to coalesce across tiers, and such rows
//!   are never counted as proof of a distinct set.
//! - Only rows with NO uid reach the legacy tuple
//!   `(repo_url, file_path, symbol_name, scope_hash)`. The scope_hash uses the
//!   symbol's scope chain (e.g. `module::class::method`) rather than line
//!   number -- line numbers shift when code changes above a symbol, but scope
//!   chains are stable across versions.
//!
//! # Cross-tier vault identity (nw-902)
//!
//! `vault_uid` hashes the vault's machine-local root path, so the same vault
//! indexed at two paths (laptop checkout vs server bare clone) mints two
//! different vault hashes, and the uid alone cannot say whether two vault
//! hashes are one vault. Within a tier the vault hash is kept, so distinct
//! vaults stay distinct. ACROSS tiers a (local vault L, server vault S) pair is
//! reconciled -- S's rows are rewritten to L's hash -- only on corroborating
//! evidence ([`reconcile_vault_pairs`]):
//!
//! - at least two distinct notes agree on (vault-relative path hash,
//!   lowercased title), OR
//! - one `sec:` row agrees on (relative path hash, start line, content hash).
//!   `sec:` is the only content-bearing uid; `head:` slugs are heading text
//!   and `tag:` is a name hash, so neither is evidence.
//!
//! and the pairing is one-to-one (a vault that corroborates with several
//! counterparts is left unreconciled). Inside a reconciled pair, notes still
//! need an equal lowercased title to merge; headings and tags merge by their
//! vault-relative key. Everything else stays vault-qualified: a duplicate row
//! is possible, a false merge is not -- except for the limit below.
//!
//! Limit: corroboration is evidence, not proof. Two DIFFERENT vaults on
//! different tiers that share two same-path-same-title notes (for example two
//! vaults generated from one template) or one section content hash WILL be
//! reconciled, and their same-path same-title notes will merge. When either
//! tier holds a vault hash that is neither shared nor reconciled
//! ([`reconcile_vault_pairs`] returns `true`) callers must not claim an exact
//! union. A path-independent vault fingerprint would need a persisted-uid
//! migration and is out of scope.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use nestweaver_schema::uid::{CanonicalEntityUid, parse_canonical_entity_uid};

/// Identity key for deduplicating results across local and server.
/// Uses scope_hash instead of line number for stability across versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolIdentity {
    pub repo_url: String,
    pub file_path: String,
    pub symbol_name: String,
    pub scope_hash: u64,
    /// uid-derived identity. When `Some`, the four presentation-derived
    /// fields above are left empty/zero so they cannot split one entity or
    /// merge two.
    pub entity: Option<EntityKey>,
}

/// Instance-independent identity derived from a row's uid. Equality and
/// hashing use `key` only; the other fields drive cross-tier reconciliation.
#[derive(Debug, Clone)]
pub struct EntityKey {
    /// Domain-qualified key with the instance removed (vault hash retained),
    /// or `uid:<raw>` for a uid the schema parser rejects.
    pub key: String,
    /// True only when the schema parser accepted the uid. Raw-keyed rows are
    /// never proof of a distinct entity set.
    pub canonical: bool,
    /// UID family prefix (`note`, `tag`, `head`, `sec`, `sym`, ...).
    pub family: &'static str,
    /// Local vault hash for vault-scoped families.
    pub vault: Option<String>,
    /// Vault-relative remainder (`note`: path hash; `tag`: name hash;
    /// `head`: `n:slug:line`; `sec`: `n:line:content`).
    rest: String,
    /// Lowercased, trimmed title (notes only); a merge guard inside a
    /// reconciled vault pair.
    title: Option<String>,
}

impl PartialEq for EntityKey {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for EntityKey {}
impl Hash for EntityKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl EntityKey {
    fn plain(family: &'static str, key: String) -> Self {
        Self {
            key,
            canonical: true,
            family,
            vault: None,
            rest: String::new(),
            title: None,
        }
    }

    fn raw(uid: &str) -> Self {
        Self {
            key: format!("uid:{uid}"),
            canonical: false,
            ..Self::plain("raw", String::new())
        }
    }

    fn scoped(family: &'static str, vault: &str, rest: String, title: Option<&str>) -> Self {
        let mut key = Self {
            key: String::new(),
            canonical: true,
            family,
            vault: Some(vault.to_string()),
            rest,
            title: title
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty()),
        };
        key.requalify();
        key
    }

    /// Replace the key (non-vault-scoped families only).
    pub fn with_key(mut self, key: String) -> Self {
        self.key = key;
        self
    }

    /// Rebuild `key` from `vault` (after a pair reconciliation rewrote it).
    fn requalify(&mut self) {
        if let Some(vault) = &self.vault {
            self.key = format!("{}:vlt:{}:{}", self.family, vault, self.rest);
        }
    }
}

/// Key a uid the schema parser accepts; `None` when it does not (caller falls
/// back to [`EntityKey::raw`]). `scope_chain` refines a `sym:` key (the uid's
/// line shifts with edits; the chain does not); `title` is kept for notes.
pub fn entity_key_for_uid(
    uid: &str,
    scope_chain: Option<&str>,
    title: Option<&str>,
) -> Option<EntityKey> {
    use CanonicalEntityUid as U;
    Some(match parse_canonical_entity_uid(uid)? {
        U::Note { vault, note, .. } => EntityKey::scoped("note", vault, note.to_string(), title),
        U::Tag { vault, name, .. } => EntityKey::scoped("tag", vault, name.to_string(), None),
        U::Heading {
            vault,
            note,
            slug,
            line,
            ..
        } => EntityKey::scoped("head", vault, format!("{note}:{slug}:{line}"), None),
        U::Section {
            vault,
            note,
            line,
            content,
            ..
        } => EntityKey::scoped("sec", vault, format!("{note}:{line}:{content}"), None),
        U::Vault { vault, .. } => EntityKey::plain("vlt", format!("vlt:{vault}")),
        U::Symbol {
            repo,
            file,
            name,
            line,
            ..
        } => {
            let disc = match scope_chain.filter(|c| !c.is_empty()) {
                Some(chain) => format!("s{}", compute_scope_hash(Some(chain), None)),
                None => format!("l{line}"),
            };
            EntityKey::plain("sym", format!("sym:repo:{repo}:{file}:{name}:{disc}"))
        }
        U::Repo { repo, .. } => EntityKey::plain("repo", format!("repo:{repo}")),
        U::File { repo, file, .. } => EntityKey::plain("file", format!("file:repo:{repo}:{file}")),
        U::Service { repo, name, .. } => EntityKey::plain("svc", format!("svc:repo:{repo}:{name}")),
    })
}

/// Reconcile local and server vault hashes (nw-902; rule in the module docs).
///
/// Rewrites the server rows of every corroborated, one-to-one (local vault,
/// server vault) pair to the local vault hash and, inside a reconciled pair,
/// adds the lowercased title to note keys. Returns `true` when, afterwards,
/// both tiers hold vault-scoped rows and some vault hash is held by only one
/// tier -- i.e. the union cannot be proven exact.
pub fn reconcile_vault_pairs(local: &mut [&mut EntityKey], server: &mut [&mut EntityKey]) -> bool {
    let vaults = |rows: &[&mut EntityKey]| -> HashSet<String> {
        rows.iter().filter_map(|k| k.vault.clone()).collect()
    };
    let local_vaults = vaults(local);
    let server_vaults = vaults(server);
    if local_vaults.is_empty() || server_vaults.is_empty() {
        return false;
    }

    // Evidence index over local vaults that no server row shares verbatim.
    let mut notes: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut secs: HashMap<String, HashSet<String>> = HashMap::new();
    for k in local.iter() {
        let Some(v) = k.vault.as_ref().filter(|v| !server_vaults.contains(*v)) else {
            continue;
        };
        match (k.family, &k.title) {
            ("note", Some(t)) => {
                notes
                    .entry((k.rest.clone(), t.clone()))
                    .or_default()
                    .insert(v.clone());
            }
            ("sec", _) => {
                secs.entry(k.rest.clone()).or_default().insert(v.clone());
            }
            _ => {}
        }
    }
    // (local vault, server vault) -> (distinct agreeing notes, section match)
    let mut pairs: HashMap<(String, String), (HashSet<String>, bool)> = HashMap::new();
    for k in server.iter() {
        let Some(s) = k.vault.as_ref().filter(|v| !local_vaults.contains(*v)) else {
            continue;
        };
        match (k.family, &k.title) {
            ("note", Some(t)) => {
                for l in notes
                    .get(&(k.rest.clone(), t.clone()))
                    .into_iter()
                    .flatten()
                {
                    pairs
                        .entry((l.clone(), s.clone()))
                        .or_default()
                        .0
                        .insert(k.rest.clone());
                }
            }
            ("sec", _) => {
                for l in secs.get(&k.rest).into_iter().flatten() {
                    pairs.entry((l.clone(), s.clone())).or_default().1 = true;
                }
            }
            _ => {}
        }
    }
    let corroborated: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(_, (n, sec))| *sec || n.len() >= 2)
        .map(|(pair, _)| pair)
        .collect();
    let mut by_local: HashMap<&str, usize> = HashMap::new();
    let mut by_server: HashMap<&str, usize> = HashMap::new();
    for (l, s) in &corroborated {
        *by_local.entry(l).or_default() += 1;
        *by_server.entry(s).or_default() += 1;
    }
    let reconciled: HashMap<String, String> = corroborated
        .iter()
        .filter(|(l, s)| by_local[l.as_str()] == 1 && by_server[s.as_str()] == 1)
        .map(|(l, s)| (s.clone(), l.clone()))
        .collect();
    let reconciled_locals: HashSet<&String> = reconciled.values().collect();

    for k in server.iter_mut() {
        if let Some(l) = k.vault.as_ref().and_then(|v| reconciled.get(v)) {
            k.vault = Some(l.clone());
            k.requalify();
        }
    }
    let guard_titles = |rows: &mut [&mut EntityKey]| {
        for k in rows.iter_mut() {
            if k.family == "note"
                && k.vault
                    .as_ref()
                    .is_some_and(|v| reconciled_locals.contains(v))
            {
                k.key = format!("{}|t:{}", k.key, k.title.as_deref().unwrap_or(""));
            }
        }
    };
    guard_titles(local);
    guard_titles(server);
    vaults(local) != vaults(server)
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
    // nw-898: a uid IS the identity. Title/location are presentation and must
    // not decide whether two rows are the same entity. A non-empty uid the
    // schema parser rejects is keyed by its raw string (never by title); only
    // uid-less rows reach the legacy tuple below.
    if let Some(uid) = result
        .get("uid")
        .and_then(|v| v.as_str())
        .filter(|u| !u.is_empty())
    {
        let entity = entity_key_for_uid(
            uid,
            result
                .get("scope_chain")
                .or_else(|| result.get("scope"))
                .and_then(|v| v.as_str()),
            result.get("title").and_then(|v| v.as_str()),
        )
        .unwrap_or_else(|| EntityKey::raw(uid));
        return Some(SymbolIdentity {
            repo_url: String::new(),
            file_path: String::new(),
            symbol_name: String::new(),
            scope_hash: 0,
            entity: Some(entity),
        });
    }

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
    // repo info.
    //
    // When a raw clone URL is present, key on its NORMALIZED identity
    // (`host/owner/name`, invariant to scheme, credentials, `.git` suffix,
    // trailing slash, and case) rather than the raw string. Otherwise the same
    // repo indexed by a LOCAL daemon under one URL form and by a SERVER under
    // another never coalesces, and shared symbols appear twice in merged
    // results instead of once with `both` provenance.
    let repo_url = result
        .get("repo_url")
        .or_else(|| result.get("repo"))
        .and_then(|v| v.as_str())
        .map(crate::repo_identity::normalized_repo_key)
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
        entity: None,
    })
}

/// Identify every row of both tiers, reconciling vault-scoped entities across
/// tiers ([`reconcile_vault_pairs`]). The vectors are index-aligned with the
/// inputs; the flag is `true` when the cross-tier vault mapping is unproven
/// and callers must not claim an exact union.
pub fn identify_tiers(
    local: &[serde_json::Value],
    server: &[serde_json::Value],
) -> (
    Vec<Option<SymbolIdentity>>,
    Vec<Option<SymbolIdentity>>,
    bool,
) {
    let mut local_ids: Vec<Option<SymbolIdentity>> = local.iter().map(extract_identity).collect();
    let mut server_ids: Vec<Option<SymbolIdentity>> = server.iter().map(extract_identity).collect();
    let mut local_keys: Vec<&mut EntityKey> = local_ids
        .iter_mut()
        .flatten()
        .filter_map(|id| id.entity.as_mut())
        .collect();
    let mut server_keys: Vec<&mut EntityKey> = server_ids
        .iter_mut()
        .flatten()
        .filter_map(|id| id.entity.as_mut())
        .collect();
    let unproven = reconcile_vault_pairs(&mut local_keys, &mut server_keys);
    (local_ids, server_ids, unproven)
}

/// True when every row carries a canonical (schema-parsed) uid identity and no
/// two rows share one -- i.e. the rows are a proven-distinct set of entities.
/// Raw-keyed (`uid:<raw>`) rows are never proof.
pub fn has_unique_canonical_identities(items: &[serde_json::Value]) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().all(|item| {
        extract_identity(item)
            .and_then(|id| id.entity)
            .is_some_and(|e| e.canonical && seen.insert(e.key))
    })
}

/// Weight multiplier for local results, matching production RRF merge.
/// See `merge.rs::LOCAL_WEIGHT`.
#[cfg(test)]
const LOCAL_WEIGHT: f64 = 1.5;

/// RRF smoothing constant, matching production. See `merge.rs::RRF_K`.
#[cfg(test)]
const RRF_K: f64 = 60.0;

/// Deduplicate results from local and server sources.
/// Local wins on content when both have the same symbol.
/// Uses the same weighted RRF scoring as production (`LOCAL_WEIGHT=1.5`).
#[cfg(test)]
pub fn deduplicate(
    local_results: Vec<serde_json::Value>,
    server_results: Vec<serde_json::Value>,
) -> Vec<MergedResult> {
    use std::collections::HashMap;
    let mut seen: HashMap<SymbolIdentity, MergedResult> = HashMap::new();
    let mut unkeyed: Vec<MergedResult> = Vec::new();

    // Insert local results first (they win on content).
    // Local results get LOCAL_WEIGHT multiplier, consistent with production merge.
    let (local_ids, server_ids, _) = identify_tiers(&local_results, &server_results);
    for (rank, (val, identity)) in local_results.into_iter().zip(local_ids).enumerate() {
        let result = MergedResult {
            value: val.clone(),
            provenance: Provenance::Local,
            confidence: Confidence::Precise,
            score: LOCAL_WEIGHT / (rank as f64 + RRF_K + 1.0),
        };
        match identity {
            Some(id) => {
                seen.insert(id, result);
            }
            None => {
                unkeyed.push(result);
            }
        }
    }

    // Insert server results, merging with existing local entries.
    // Server results use baseline weight (1.0).
    for (rank, (val, identity)) in server_results.into_iter().zip(server_ids).enumerate() {
        match identity {
            Some(id) => {
                if let Some(existing) = seen.get_mut(&id) {
                    // Both have it — local wins on content, mark as Both
                    existing.provenance = Provenance::Both;
                    // Add server's RRF score (unweighted)
                    existing.score += 1.0 / (rank as f64 + RRF_K + 1.0);
                } else {
                    // Server-only
                    seen.insert(
                        id,
                        MergedResult {
                            value: val,
                            provenance: Provenance::Server,
                            confidence: Confidence::Precise,
                            score: 1.0 / (rank as f64 + RRF_K + 1.0),
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
                    score: 1.0 / (rank as f64 + RRF_K + 1.0),
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

    /// nw-898: every canonical uid family is keyed by the uid itself, with
    /// the instance removed and presentation fields (title is only a note
    /// guard inside a reconciled vault pair, never part of the base key)
    /// ignored.
    #[test]
    fn canonical_uid_is_the_identity_for_every_family() {
        use nestweaver_schema::uid::*;
        let vault = vault_uid("inst-a", "/some/root");
        let note = note_uid(&vault, "a.md");
        let repo = repo_uid("inst-a", "https://example.com/acme/api.git");
        let uids = [
            note.clone(),
            tag_uid(&vault, "zebra"),
            heading_uid(&note, "intro", 4),
            section_uid(&note, 4, "abc123"),
            vault.clone(),
            symbol_uid(&repo, "src/lib.rs", "f", 9),
            repo.clone(),
            file_uid(&repo, "src/lib.rs"),
            service_uid(&repo, "svc"),
        ];
        let mut keys = HashSet::new();
        for uid in &uids {
            let row = |title: &str, loc: &str| json!({"uid": uid, "title": title, "location": loc});
            let a = extract_identity(&row("Same", "x:1")).unwrap();
            let c = extract_identity(&row("Same", "x:2")).unwrap();
            assert!(a.entity.as_ref().is_some_and(|e| e.canonical), "{uid}");
            assert_eq!(a, extract_identity(&row("Same", "x:1")).unwrap());
            assert_eq!(a, c, "location must not matter for {uid}");
            assert!(keys.insert(a), "{uid} collided with another family");
        }
        // Instance never matters.
        let other = tag_uid(&vault_uid("inst-b", "/some/root"), "zebra");
        assert_eq!(
            extract_identity(&json!({"uid": other, "title": "t"})).unwrap(),
            extract_identity(&json!({"uid": uids[1], "title": "t"})).unwrap()
        );
    }

    /// nw-898 (R4): a uid the schema parser rejects is keyed by its raw string,
    /// never by title: two distinct `proj:`/`contract:` rows with one title stay
    /// two rows, are not "canonical" proof, and a repeat of the same raw uid
    /// still coalesces. Rows with NO uid still survive via the legacy tuple.
    #[test]
    fn unrecognized_uids_key_on_the_raw_string_and_never_prove_uniqueness() {
        let rows = vec![
            json!({"uid": "proj:inst:abcdef012345", "title": "Shared"}),
            json!({"uid": "contract:http:GET:/a", "title": "Shared"}),
        ];
        let merged = crate::merge::rrf_merge(rows.clone(), vec![rows[0].clone()]);
        assert_eq!(merged.len(), 2, "distinct raw uids must not collapse");
        assert_eq!(
            merged
                .iter()
                .filter(|m| m.provenance == Provenance::Both)
                .count(),
            1
        );
        assert!(!has_unique_canonical_identities(&rows));
        // No uid at all: still kept (legacy tuple), distinct from a uid row.
        let plain = json!({"name": "a", "file": "a.rs"});
        let merged = crate::merge::rrf_merge(vec![plain.clone()], vec![rows[0].clone()]);
        assert_eq!(merged.len(), 2);
    }

    // ── nw-902: corroborated cross-tier vault reconciliation ─────────

    fn note_row(instance: &str, root: &str, rel: &str, title: &str) -> serde_json::Value {
        use nestweaver_schema::uid::*;
        json!({"uid": note_uid(&vault_uid(instance, root), rel), "title": title})
    }
    fn sec_row(instance: &str, root: &str, rel: &str, line: u32, c: &str) -> serde_json::Value {
        use nestweaver_schema::uid::*;
        let n = note_uid(&vault_uid(instance, root), rel);
        json!({"uid": section_uid(&n, line, c), "title": "s"})
    }
    fn head_row(instance: &str, root: &str, rel: &str, slug: &str) -> serde_json::Value {
        use nestweaver_schema::uid::*;
        let n = note_uid(&vault_uid(instance, root), rel);
        json!({"uid": heading_uid(&n, slug, 1), "title": "h"})
    }
    fn tag_row(instance: &str, root: &str, name: &str) -> serde_json::Value {
        use nestweaver_schema::uid::*;
        json!({"uid": tag_uid(&vault_uid(instance, root), name), "title": name})
    }
    /// (merged row count, unproven flag) for one local + one server list.
    fn reconcile(local: Vec<serde_json::Value>, server: Vec<serde_json::Value>) -> (usize, bool) {
        let (_, _, unproven) = identify_tiers(&local, &server);
        (crate::merge::rrf_merge(local, server).len(), unproven)
    }

    #[test]
    fn two_agreeing_notes_reconcile_a_vault_pair_and_carry_heads_and_tags() {
        let local = vec![
            note_row("l", "/a", "x.md", "X"),
            note_row("l", "/a", "y.md", "Y"),
            head_row("l", "/a", "x.md", "intro"),
            tag_row("l", "/a", "todo"),
        ];
        let server = vec![
            note_row("s", "/b", "x.md", "x"),
            note_row("s", "/b", "y.md", "Y"),
            head_row("s", "/b", "x.md", "intro"),
            tag_row("s", "/b", "todo"),
        ];
        assert_eq!(reconcile(local, server), (4, false));
    }

    /// A single agreeing note (README/Home/Inbox) is NOT evidence: two
    /// different vaults must stay two rows and the union stay unproven.
    #[test]
    fn one_agreeing_note_never_reconciles_vaults() {
        let local = vec![note_row("l", "/a", "README.md", "README")];
        let server = vec![note_row("s", "/b", "README.md", "README")];
        assert_eq!(reconcile(local, server), (2, true));
    }

    /// Heads and tags alone never corroborate.
    #[test]
    fn heads_and_tags_alone_never_corroborate() {
        let local = vec![
            head_row("l", "/a", "x.md", "intro"),
            head_row("l", "/a", "y.md", "intro"),
            tag_row("l", "/a", "todo"),
            tag_row("l", "/a", "wip"),
        ];
        let server = vec![
            head_row("s", "/b", "x.md", "intro"),
            head_row("s", "/b", "y.md", "intro"),
            tag_row("s", "/b", "todo"),
            tag_row("s", "/b", "wip"),
        ];
        assert_eq!(reconcile(local, server), (8, true));
    }

    /// One matching `sec:` (content hash) corroborates alone, then carries the
    /// vault's notes (equal title), headings and tags.
    #[test]
    fn one_matching_section_reconciles_the_whole_pair() {
        let local = vec![
            sec_row("l", "/a", "x.md", 3, "abc123"),
            note_row("l", "/a", "x.md", "X"),
            head_row("l", "/a", "x.md", "intro"),
            tag_row("l", "/a", "todo"),
        ];
        let server = vec![
            sec_row("s", "/b", "x.md", 3, "abc123"),
            note_row("s", "/b", "x.md", "X"),
            head_row("s", "/b", "x.md", "intro"),
            tag_row("s", "/b", "todo"),
        ];
        assert_eq!(reconcile(local, server), (4, false));
        // A different content hash is no evidence.
        let local = vec![sec_row("l", "/a", "x.md", 3, "abc123")];
        let server = vec![sec_row("s", "/b", "x.md", 3, "fff999")];
        assert_eq!(reconcile(local, server), (2, true));
    }

    /// Inside a reconciled pair a note needs an equal (case-insensitive)
    /// title: a differing title stays two rows.
    #[test]
    fn reconciled_pair_still_requires_equal_note_titles() {
        let local = vec![
            note_row("l", "/a", "x.md", "X"),
            note_row("l", "/a", "y.md", "Y"),
            note_row("l", "/a", "z.md", "Zed"),
        ];
        let server = vec![
            note_row("s", "/b", "x.md", "x"),
            note_row("s", "/b", "y.md", "Y"),
            note_row("s", "/b", "z.md", "Zebra"),
        ];
        // x (case) and y merge; z has differing titles -> two rows.
        assert_eq!(reconcile(local, server), (4, false));
    }

    /// One local vault corroborating with two server vaults is ambiguous:
    /// nothing is reconciled.
    #[test]
    fn a_vault_matching_two_counterparts_is_not_reconciled() {
        let local = vec![
            note_row("l", "/a", "x.md", "X"),
            note_row("l", "/a", "y.md", "Y"),
        ];
        let server = vec![
            note_row("s", "/b", "x.md", "X"),
            note_row("s", "/b", "y.md", "Y"),
            note_row("s", "/c", "x.md", "X"),
            note_row("s", "/c", "y.md", "Y"),
        ];
        assert_eq!(reconcile(local, server), (6, true));
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
    fn cross_instance_dedup_reconciles_ssh_and_https_url_forms() {
        // Cross-instance fixture (T3.0 RED anchor): the LOCAL daemon indexed the
        // repo from an ssh clone URL while the SERVER indexed the *same* repo
        // from its canonical https URL. Same symbol, same file, same scope —
        // only the repo URL *form* differs (scheme, credentials, `.git` suffix).
        // Result identity must key on the NORMALIZED repo identity so the two
        // rows coalesce into a single `both` row instead of appearing twice as
        // separate local + server rows.
        let local = vec![json!({
            "repo_url": "git@github.com:acme/api.git",
            "file_path": "src/billing.rs",
            "symbol_name": "process_payment",
            "scope_chain": "billing::process_payment",
        })];
        let server = vec![json!({
            "repo_url": "https://github.com/acme/api",
            "file_path": "src/billing.rs",
            "symbol_name": "process_payment",
            "scope_chain": "billing::process_payment",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(
            merged.len(),
            1,
            "same symbol under two URL forms must dedup to a single row"
        );
        assert_eq!(
            merged[0].provenance,
            Provenance::Both,
            "reconciled symbol must be attributed to both sources"
        );
    }

    #[test]
    fn cross_instance_dedup_trailing_slash_and_case_reconcile() {
        // Trailing slash + host/owner casing differences must not split identity.
        let local = vec![json!({
            "repo_url": "https://GitHub.com/Acme/API/",
            "file_path": "src/lib.rs",
            "symbol_name": "handler",
            "scope_chain": "api::handler",
        })];
        let server = vec![json!({
            "repo_url": "https://github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "handler",
            "scope_chain": "api::handler",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Both);
    }

    #[test]
    fn cross_instance_distinct_repos_do_not_merge() {
        // Two genuinely different repos that happen to share a symbol/file must
        // NOT be merged — normalization must not over-collapse distinct owners.
        let local = vec![json!({
            "repo_url": "https://github.com/acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "handler",
            "scope_chain": "api::handler",
        })];
        let server = vec![json!({
            "repo_url": "https://github.com/other/api",
            "file_path": "src/lib.rs",
            "symbol_name": "handler",
            "scope_chain": "api::handler",
        })];

        let merged = deduplicate(local, server);
        assert_eq!(merged.len(), 2, "distinct owners must remain distinct");
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
        // Combined score should be higher than a single source.
        // Local alone: LOCAL_WEIGHT / (0 + RRF_K + 1) = 1.5 / 61.0
        let single_local_score = LOCAL_WEIGHT / (RRF_K + 1.0);
        assert!(merged[0].score > single_local_score);
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

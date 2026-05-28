use std::collections::{HashMap, HashSet};

use nestweaver_store::GraphStore;
use serde::Serialize;

use crate::config::LinkConfig;
use crate::cross_domain::STOPLIST;
use crate::manifest::ManifestInfo;
use crate::repo_display_name;

/// Confidence level of a suggested cross-repo link.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::High => write!(f, "high"),
            Self::Medium => write!(f, "medium"),
            Self::Low => write!(f, "low"),
        }
    }
}

/// A suggested cross-repo link between two repos.
#[derive(Debug, Serialize)]
pub struct SuggestedLink {
    pub from: String,
    pub to: String,
    pub link_type: String,
    pub description: String,
    pub shared_symbols: Vec<String>,
    pub confidence: Confidence,
}

/// A suggested feature bundle spanning multiple repos.
#[derive(Debug, Serialize)]
pub struct SuggestedFeature {
    pub name: String,
    pub description: String,
    pub repos: Vec<String>,
    pub entry_points: Vec<String>,
}

/// The full set of suggestions returned by `suggest_links`.
pub struct Suggestions {
    pub links: Vec<SuggestedLink>,
    pub features: Vec<SuggestedFeature>,
}

/// Symbol names that are too generic to be meaningful cross-repo signals.
const NOISE_NAMES: &[&str] = &[
    "default",
    "new",
    "get",
    "set",
    "init",
    "main",
    "app",
    "index",
    "render",
    "test",
    "setup",
    "config",
    "error",
    "handle",
    "create",
    "update",
    "delete",
    "fetch",
    "use",
    "export",
    "import",
    "run",
    "start",
    "stop",
    "close",
    "open",
    "load",
    "save",
    "read",
    "write",
    "send",
    "receive",
    "parse",
    "format",
    "build",
    "list",
    "add",
    "remove",
    "tostring",
    "equals",
    "hashcode",
    "valueof",
    "constructor",
    "prototype",
    "__init__",
    "__str__",
    "apply",
    "call",
    "bind",
];

/// Common framework/UI pattern suffixes that are not evidence of a real relationship.
const FRAMEWORK_SUFFIXES: &[&str] = &[
    "Props",
    "State",
    "Context",
    "Provider",
    "Consumer",
    "Handler",
    "Wrapper",
    "Container",
    "Component",
    "Screen",
    "Page",
    "Modal",
    "Dialog",
    "Button",
    "Input",
    "Form",
    "List",
    "Item",
    "Card",
    "Icon",
    "Loader",
    "Spinner",
    "Skeleton",
    "Layout",
    "Header",
    "Footer",
    "Sidebar",
    "Nav",
    "Menu",
    "Tab",
    "Badge",
    "Avatar",
    "Tooltip",
    "Type",
    "Error",
    "Result",
    "Response",
    "Request",
    "Config",
    "Options",
    "Params",
    "Args",
    "Ref",
    "Hook",
    "Dispatch",
];

/// Full names that are common React/framework patterns but not caught by suffix matching.
const FRAMEWORK_NAMES: &[&str] = &[
    "useAuth",
    "useRouter",
    "useNavigate",
    "useEffect",
    "useState",
    "useContext",
    "useCallback",
    "useMemo",
    "useRef",
    "useParams",
    "useLocation",
    "useHistory",
    "useDispatch",
    "useSelector",
    "useQuery",
    "useMutation",
    "handleChange",
    "handleSubmit",
    "handleClick",
    "handleClose",
    "handleOpen",
    "handleSave",
    "handleDelete",
    "handleCancel",
    "handleRestore",
    "handleAction",
    "handleError",
    "componentDidCatch",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "getDerivedStateFromError",
    "getDerivedStateFromProps",
    "shouldComponentUpdate",
    "navigate",
    "formatDate",
    "formatCurrency",
    "formatNumber",
    "compareVersions",
    "clearFilters",
    "addRow",
    "deleteUser",
    "Login",
    "Dashboard",
    "User",
    "Date",
    "String",
    "Map",
    "Notification",
    "Filters",
    "Contact",
    "Product",
];

fn is_noise(name: &str) -> bool {
    let lower = name.to_lowercase();
    if NOISE_NAMES.contains(&lower.as_str()) || name.len() < 3 {
        return true;
    }
    // Single common words (no camelCase or domain specificity)
    if name.len() <= 6 && name.chars().all(|c| c.is_lowercase() || c.is_ascii_digit()) {
        return true;
    }
    false
}

fn is_framework_pattern(name: &str) -> bool {
    if FRAMEWORK_NAMES.contains(&name) {
        return true;
    }
    FRAMEWORK_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix) && name.len() <= suffix.len() + 12)
}

/// Analyze all indexed repos and suggest cross-repo `[[links]]` and `[[features]]`.
///
/// Signal 1 (high confidence): manifest-declared package dependencies.
/// Signal 2 (low confidence): IDF-filtered shared symbol names.
///
/// Manifest links are emitted first so callers can present the most reliable
/// suggestions at the top.
pub fn suggest_links(
    store: &GraphStore,
    manifests: &HashMap<String, ManifestInfo>,
) -> Result<Suggestions, anyhow::Error> {
    let repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;

    if repos.len() < 2 {
        return Ok(Suggestions {
            links: vec![],
            features: vec![],
        });
    }

    let mut uid_to_name: HashMap<String, String> = HashMap::new();
    for repo in &repos {
        let display_name = repo_display_name(repo);
        uid_to_name.insert(repo.uid.clone(), display_name);
    }

    // ── Signal 1: Manifest dependencies (high confidence) ────────────────────
    let mut manifest_links: Vec<SuggestedLink> = Vec::new();
    let manifest_uids: Vec<&String> = manifests.keys().collect();
    for uid_a in &manifest_uids {
        let manifest_a = &manifests[*uid_a];
        for uid_b in &manifest_uids {
            if uid_a == uid_b {
                continue;
            }
            let manifest_b = &manifests[*uid_b];
            let Some(pkg_name) = &manifest_b.package_name else {
                continue;
            };
            // A depends on B when any of A's dep strings equals B's package name
            // or ends with "/<pkg_name>" (Go-style scoped paths).
            let depends = manifest_a
                .dependencies
                .iter()
                .any(|dep| dep == pkg_name || dep.ends_with(&format!("/{pkg_name}")));
            if depends {
                let name_a = uid_to_name
                    .get(*uid_a)
                    .cloned()
                    .unwrap_or_else(|| (*uid_a).clone());
                let name_b = uid_to_name
                    .get(*uid_b)
                    .cloned()
                    .unwrap_or_else(|| (*uid_b).clone());
                manifest_links.push(SuggestedLink {
                    from: name_a,
                    to: name_b,
                    link_type: "package-dependency".to_string(),
                    description: format!("Depends on {} (from manifest)", pkg_name),
                    shared_symbols: vec![pkg_name.clone()],
                    confidence: Confidence::High,
                });
            }
        }
    }

    // ── Signal 2: IDF-filtered shared symbol names (low confidence) ──────────
    let mut repo_symbols: HashMap<String, HashSet<String>> = HashMap::new();
    // Track which repos each symbol name appears in (for IDF filtering)
    let mut name_to_repos: HashMap<String, HashSet<String>> = HashMap::new();

    for repo in &repos {
        let names = store
            .symbol_names_by_repo(&repo.uid)
            .map_err(|e| anyhow::anyhow!(e))?;
        for name in &names {
            name_to_repos
                .entry(name.clone())
                .or_default()
                .insert(repo.uid.clone());
        }
        repo_symbols
            .entry(repo.uid.clone())
            .or_default()
            .extend(names);
    }

    // IDF threshold: a name appearing in too many repos is generic.
    // Use the smaller of 30% of repos or 3.
    let idf_max = (repos.len() as f64 * 0.3).ceil().max(3.0) as usize;

    let repo_uids: Vec<String> = repo_symbols.keys().cloned().collect();
    let mut name_links: Vec<SuggestedLink> = Vec::new();

    for i in 0..repo_uids.len() {
        for j in (i + 1)..repo_uids.len() {
            let uid_a = &repo_uids[i];
            let uid_b = &repo_uids[j];
            let syms_a = &repo_symbols[uid_a];
            let syms_b = &repo_symbols[uid_b];

            let mut shared: Vec<String> = syms_a
                .intersection(syms_b)
                .filter(|name| {
                    if is_noise(name) {
                        return false;
                    }
                    if is_framework_pattern(name) {
                        return false;
                    }
                    // IDF filter: skip names that appear in too many repos
                    let repo_count = name_to_repos.get(*name).map(|s| s.len()).unwrap_or(0);
                    repo_count <= idf_max
                })
                .cloned()
                .collect();
            shared.sort();

            // Require at least one "high-specificity" symbol: 15+ chars or
            // contains 3+ camelCase/snake_case segments. This prevents links
            // based solely on common short names like "Event", "Location".
            let has_specific = shared.iter().any(|name| {
                if name.len() >= 15 {
                    return true;
                }
                let segments = name
                    .chars()
                    .filter(|c| c.is_uppercase() || *c == '_')
                    .count();
                segments >= 2
            });

            if shared.len() >= 2 && has_specific {
                let confidence = if shared.len() >= 5 {
                    Confidence::High
                } else if shared.len() >= 3 {
                    Confidence::Medium
                } else {
                    Confidence::Low
                };

                let preview: Vec<&str> = shared.iter().take(5).map(|s| s.as_str()).collect();
                let description = format!("Both repos reference: {}", preview.join(", "));

                let name_a = uid_to_name
                    .get(uid_a)
                    .cloned()
                    .unwrap_or_else(|| uid_a.clone());
                let name_b = uid_to_name
                    .get(uid_b)
                    .cloned()
                    .unwrap_or_else(|| uid_b.clone());

                name_links.push(SuggestedLink {
                    from: name_a,
                    to: name_b,
                    link_type: "shared-types".to_string(),
                    description,
                    shared_symbols: shared,
                    confidence,
                });
            }
        }
    }

    // Manifest-based links first, then name-based.
    let mut suggested_links = manifest_links;
    suggested_links.extend(name_links);

    // Build feature suggestions from links with enough shared symbols.
    let mut suggested_features: Vec<SuggestedFeature> = Vec::new();
    for link in &suggested_links {
        if link.shared_symbols.len() >= 3 {
            // Pick the longest shared symbol name as a feature name seed.
            let feature_name = link
                .shared_symbols
                .iter()
                .max_by_key(|s| s.len())
                .cloned()
                .unwrap_or_else(|| format!("{}-{}-shared", link.from, link.to));

            suggested_features.push(SuggestedFeature {
                name: feature_name.to_lowercase().replace(' ', "-"),
                description: format!("Shared functionality between {} and {}", link.from, link.to),
                repos: vec![link.from.clone(), link.to.clone()],
                entry_points: link.shared_symbols.iter().take(5).cloned().collect(),
            });
        }
    }

    Ok(Suggestions {
        links: suggested_links,
        features: suggested_features,
    })
}

// ── Cross-repo symbol-level link discovery ─────────────────────────────────

/// Minimum symbol name length to consider for cross-repo contract matching.
/// Below this threshold names collide too easily with English words and
/// short identifiers that appear in every codebase.
const MIN_CROSS_REPO_NAME_LEN: usize = 6;

/// Find symbols that share the same name across two or more repos and
/// return them as `SuggestedLink` entries with `link_type = "shared-symbol"`.
///
/// Filtering rules (applied before any pairing):
/// - Name shorter than `MIN_CROSS_REPO_NAME_LEN` → skipped.
/// - Name present in the cross-domain `STOPLIST` (case-insensitive) → skipped.
///
/// Confidence:
/// - `High` if the two symbols have the same kind (both Function, both Class, …).
/// - `Medium` otherwise.
///
/// Deduplication: the caller is responsible for merging these with any
/// manifest-level links that already exist for the same pair of repos.
pub fn discover_symbol_level_links(
    store: &GraphStore,
) -> Result<Vec<SuggestedLink>, anyhow::Error> {
    let repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;
    if repos.len() < 2 {
        return Ok(vec![]);
    }

    let stop_set: HashSet<&str> = STOPLIST.iter().copied().collect();

    // Build a map: name → list of (repo_uid, sym_uid, kind) for every
    // symbol in every repo. We load symbols lazily per repo to avoid one
    // giant allocation when there are many repos.
    let mut name_to_entries: HashMap<String, Vec<(String, String, String)>> = HashMap::new();

    for repo in &repos {
        let symbols = store
            .symbol_lite_by_repo(&repo.uid)
            .map_err(|e| anyhow::anyhow!(e))?;
        for (sym_uid, name, kind) in symbols {
            if name.len() < MIN_CROSS_REPO_NAME_LEN {
                continue;
            }
            if stop_set.contains(name.to_ascii_lowercase().as_str()) {
                continue;
            }
            // Require identifier-shaped names only (no scope separators,
            // angle brackets, etc.) to match the cross-domain filter.
            if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            name_to_entries
                .entry(name)
                .or_default()
                .push((repo.uid.clone(), sym_uid, kind));
        }
    }

    // For each name that appears in 2+ repos, create pairwise SuggestedLinks.
    // We collect (repo_uid_a, repo_uid_b, name, kind_a, kind_b) tuples first,
    // then group by (repo pair) so we can emit one SuggestedLink per pair with
    // all shared names bundled together.
    let mut pair_to_link: HashMap<(String, String), SuggestedLink> = HashMap::new();

    for (name, entries) in &name_to_entries {
        // Collect distinct repos that have this name.
        let mut repos_for_name: Vec<(&str, &str, &str)> = Vec::new(); // (repo_uid, sym_uid, kind)
        let mut seen_repos: HashSet<&str> = HashSet::new();
        for (repo_uid, sym_uid, kind) in entries {
            if seen_repos.insert(repo_uid.as_str()) {
                repos_for_name.push((repo_uid.as_str(), sym_uid.as_str(), kind.as_str()));
            }
        }
        if repos_for_name.len() < 2 {
            continue;
        }

        // Emit a link for every distinct pair (i, j) with i < j.
        for i in 0..repos_for_name.len() {
            for j in (i + 1)..repos_for_name.len() {
                let (repo_a, _, kind_a) = repos_for_name[i];
                let (repo_b, _, kind_b) = repos_for_name[j];

                // Canonical ordering so (A, B) and (B, A) merge into the same key.
                let key = if repo_a <= repo_b {
                    (repo_a.to_string(), repo_b.to_string())
                } else {
                    (repo_b.to_string(), repo_a.to_string())
                };

                let confidence = if kind_a == kind_b {
                    Confidence::High
                } else {
                    Confidence::Medium
                };

                let entry = pair_to_link.entry(key.clone()).or_insert_with(|| {
                    SuggestedLink {
                        from: key.0.clone(),
                        to: key.1.clone(),
                        link_type: "shared-symbol".to_string(),
                        description: String::new(), // filled below
                        shared_symbols: Vec::new(),
                        confidence,
                    }
                });

                if !entry.shared_symbols.contains(name) {
                    entry.shared_symbols.push(name.clone());
                    // Upgrade confidence when any pair has matching kinds.
                    if matches!(confidence, Confidence::High) {
                        entry.confidence = Confidence::High;
                    }
                }
            }
        }
    }

    // Populate description now that shared_symbols is complete.
    let mut links: Vec<SuggestedLink> = pair_to_link
        .into_values()
        .map(|mut link| {
            link.shared_symbols.sort();
            let preview: Vec<&str> = link
                .shared_symbols
                .iter()
                .take(5)
                .map(String::as_str)
                .collect();
            link.description = format!("Shared symbol names: {}", preview.join(", "));
            link
        })
        .collect();

    // Stable ordering: sort by (from, to) for deterministic output.
    links.sort_by(|a, b| a.from.cmp(&b.from).then(a.to.cmp(&b.to)));
    Ok(links)
}

/// Persist cross-repo symbol-level links discovered by
/// `discover_symbol_level_links` into the graph as `CROSS_REPO_LINK` edges.
///
/// For each `SuggestedLink`, for each shared symbol name, the function
/// looks up all Symbol UIDs with that name in the two repos and inserts
/// directed edges between every (from-symbol, to-symbol) pair.
///
/// Returns the total number of edges inserted.
pub fn persist_cross_repo_links(
    store: &GraphStore,
    links: &[SuggestedLink],
) -> Result<usize, anyhow::Error> {
    let mut total = 0usize;
    for link in links {
        let confidence = match link.confidence {
            Confidence::High => 0.9_f32,
            Confidence::Medium => 0.7,
            Confidence::Low => 0.5,
        };

        // `link.from` and `link.to` are repo UIDs for symbol-level links.
        let repo_a = &link.from;
        let repo_b = &link.to;

        for name in &link.shared_symbols {
            // Find all symbols with this name in repo A and repo B.
            let syms_a = store
                .lookup_symbols_by_name_in_repo(name, repo_a)
                .map_err(|e| anyhow::anyhow!(e))?;
            let syms_b = store
                .lookup_symbols_by_name_in_repo(name, repo_b)
                .map_err(|e| anyhow::anyhow!(e))?;

            for (uid_a, _) in &syms_a {
                for (uid_b, _) in &syms_b {
                    store
                        .insert_cross_repo_link(uid_a, uid_b, confidence, &link.link_type)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    total += 1;
                }
            }
        }
    }
    Ok(total)
}

/// Materialize declared config links as `CROSS_REPO_LINK` graph edges.
///
/// For each `LinkConfig` with `materialize = true`, the function resolves
/// the `from` and `to` repo names against the Repo nodes in the graph, then
/// inserts directed `CROSS_REPO_LINK` edges between every (from-symbol,
/// to-symbol) pair that shares a name across those repos.
///
/// Links without `materialize = true` are silently skipped.
///
/// Returns the total number of edges inserted.
pub fn materialize_declared_links(
    store: &GraphStore,
    links: &[LinkConfig],
) -> Result<usize, anyhow::Error> {
    if links.is_empty() {
        return Ok(0);
    }

    // Build a map of repo short-name → repo_uid to resolve the from/to names.
    let all_repos = store.list_repos(None).map_err(|e| anyhow::anyhow!(e))?;
    let name_to_uid: HashMap<String, String> = all_repos
        .iter()
        .map(|r| (repo_display_name(r), r.uid.clone()))
        .collect();

    let mut total = 0usize;
    for link in links {
        if !link.materialize {
            continue;
        }
        let from_uid = match name_to_uid.get(&link.from) {
            Some(uid) => uid.clone(),
            None => {
                tracing::warn!(
                    "materialize_declared_links: repo '{}' not found in graph, skipping link",
                    link.from
                );
                continue;
            }
        };
        let to_uid = match name_to_uid.get(&link.to) {
            Some(uid) => uid.clone(),
            None => {
                tracing::warn!(
                    "materialize_declared_links: repo '{}' not found in graph, skipping link",
                    link.to
                );
                continue;
            }
        };

        // Find shared symbol names between the two repos.
        // symbol_lite_by_repo returns (uid, name, kind) triples.
        let syms_from = store
            .symbol_lite_by_repo(&from_uid)
            .map_err(|e| anyhow::anyhow!(e))?;
        let syms_to = store
            .symbol_lite_by_repo(&to_uid)
            .map_err(|e| anyhow::anyhow!(e))?;

        // Build a name → [(uid)] map for the target repo.
        let mut to_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (uid, name, _kind) in &syms_to {
            to_by_name
                .entry(name.clone())
                .or_default()
                .push(uid.clone());
        }

        // Insert an edge for every (from-symbol, to-symbol) with a shared name.
        for (from_sym_uid, name, _kind) in &syms_from {
            if let Some(to_uids) = to_by_name.get(name) {
                let conf = 0.9_f32; // declared links have high confidence
                for to_sym_uid in to_uids {
                    store
                        .insert_cross_repo_link(from_sym_uid, to_sym_uid, conf, &link.link_type)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    total += 1;
                }
            }
        }
    }
    Ok(total)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    use super::{Confidence, suggest_links};
    use crate::manifest::ManifestInfo;

    fn make_repo(uid: &str, url: &str) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: "abc".to_string(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
        }
    }

    fn make_symbol(uid: &str, name: &str, repo_uid: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        }
    }

    fn no_manifests() -> HashMap<String, ManifestInfo> {
        HashMap::new()
    }

    #[test]
    fn suggest_links_returns_empty_for_single_repo() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/alpha"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s1", "syncData", "r1"))
            .unwrap();

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();
        assert!(
            suggestions.links.is_empty(),
            "single repo should produce no suggested links"
        );
    }

    #[test]
    fn suggest_links_finds_shared_symbols_across_repos() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/alpha"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/example/beta"))
            .unwrap();

        // Insert 3 shared non-noise symbols across the two repos.
        for (uid, name, repo) in [
            ("s1", "syncFirestore", "r1"),
            ("s2", "syncFirestore", "r2"),
            ("s3", "SessionModel", "r1"),
            ("s4", "SessionModel", "r2"),
            ("s5", "UserProfile", "r1"),
            ("s6", "UserProfile", "r2"),
        ] {
            store.insert_symbol(&make_symbol(uid, name, repo)).unwrap();
        }

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();
        assert!(
            !suggestions.links.is_empty(),
            "should suggest at least one link for repos with shared symbols"
        );
        let link = &suggestions.links[0];
        assert!(
            link.shared_symbols.len() >= 3,
            "should find all 3 shared symbols"
        );
        assert!(
            matches!(link.confidence, super::Confidence::Medium),
            "expected medium confidence"
        );
        // Feature should also be suggested.
        assert!(
            !suggestions.features.is_empty(),
            "should suggest a feature for 3+ shared symbols"
        );
    }

    #[test]
    fn suggest_links_ignores_noise_names() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/alpha"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/example/beta"))
            .unwrap();

        // Only noise names — should not suggest a link.
        for (uid, name, repo) in [
            ("s1", "get", "r1"),
            ("s2", "get", "r2"),
            ("s3", "set", "r1"),
            ("s4", "set", "r2"),
            ("s5", "run", "r1"),
            ("s6", "run", "r2"),
        ] {
            store.insert_symbol(&make_symbol(uid, name, repo)).unwrap();
        }

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();
        assert!(
            suggestions.links.is_empty(),
            "noise names should not trigger a link suggestion"
        );
    }

    #[test]
    fn suggest_links_no_collision_for_same_derived_name() {
        let store = GraphStore::in_memory().unwrap();
        // Two repos that both derive the name "service" from their URLs.
        store
            .insert_repo(&make_repo("r1", "https://github.com/org1/service"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/org2/service"))
            .unwrap();

        // Give each distinct symbol sets — they share nothing meaningful.
        store
            .insert_symbol(&make_symbol("s1", "UniqueAlpha", "r1"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s2", "UniqueBeta", "r2"))
            .unwrap();

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();
        // No shared non-noise symbols — no links suggested (and no collision merging).
        assert!(
            suggestions.links.is_empty(),
            "repos with no shared symbols should produce no links, even if they share a derived name"
        );
    }

    #[test]
    fn suggest_links_filters_framework_patterns() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/org/app-a"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/org/app-b"))
            .unwrap();

        // Only framework pattern names — should NOT suggest a link
        for (uid, name, repo) in [
            ("s1", "ButtonProps", "r1"),
            ("s2", "ButtonProps", "r2"),
            ("s3", "AuthProvider", "r1"),
            ("s4", "AuthProvider", "r2"),
            ("s5", "UserContext", "r1"),
            ("s6", "UserContext", "r2"),
        ] {
            store.insert_symbol(&make_symbol(uid, name, repo)).unwrap();
        }

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();
        assert!(
            suggestions.links.is_empty(),
            "framework patterns (ButtonProps, AuthProvider, UserContext) should not trigger links"
        );
    }

    #[test]
    fn suggest_links_idf_filters_names_in_many_repos() {
        let store = GraphStore::in_memory().unwrap();
        // Create 5 repos
        for i in 1..=5 {
            store
                .insert_repo(&make_repo(
                    &format!("r{i}"),
                    &format!("https://github.com/org/repo-{i}"),
                ))
                .unwrap();
        }

        // "Event" appears in all 5 repos — should be filtered by IDF
        for i in 1..=5 {
            store
                .insert_symbol(&make_symbol(&format!("ev{i}"), "Event", &format!("r{i}")))
                .unwrap();
        }

        // "syncSpecificData" appears in only 2 repos — should be kept
        store
            .insert_symbol(&make_symbol("sp1", "syncSpecificData", "r1"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sp2", "syncSpecificData", "r2"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sp3", "processSpecificJob", "r1"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sp4", "processSpecificJob", "r2"))
            .unwrap();

        let suggestions = suggest_links(&store, &no_manifests()).unwrap();

        // Should find a link between r1 and r2 (syncSpecificData + processSpecificJob)
        // but NOT based on "Event" (appears in too many repos)
        let r1_r2_link = suggestions.links.iter().find(|l| {
            (l.from == "repo-1" && l.to == "repo-2") || (l.from == "repo-2" && l.to == "repo-1")
        });
        assert!(
            r1_r2_link.is_some(),
            "should find link between repo-1 and repo-2"
        );
        let link = r1_r2_link.unwrap();
        assert!(
            !link.shared_symbols.contains(&"Event".to_string()),
            "Event should be filtered by IDF (appears in 5/5 repos)"
        );
        assert!(
            link.shared_symbols
                .contains(&"syncSpecificData".to_string()),
            "syncSpecificData should be kept (appears in only 2 repos)"
        );
    }

    #[test]
    fn discover_symbol_level_links_finds_shared_symbols() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/service-a"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/example/service-b"))
            .unwrap();

        // "AuthService" — 11 chars, valid identifier, appears in both repos.
        store
            .insert_symbol(&make_symbol("s1", "AuthService", "r1"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s2", "AuthService", "r2"))
            .unwrap();

        let links = super::discover_symbol_level_links(&store).unwrap();
        assert!(
            !links.is_empty(),
            "should find at least one cross-repo link for shared symbol 'AuthService'"
        );
        let link = &links[0];
        assert_eq!(link.link_type, "shared-symbol");
        assert!(
            link.shared_symbols.contains(&"AuthService".to_string()),
            "shared_symbols should contain 'AuthService'"
        );
    }

    #[test]
    fn discover_symbol_level_links_skips_short_names() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/alpha"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/example/beta"))
            .unwrap();

        // "build" (5 chars) is below MIN_CROSS_REPO_NAME_LEN = 6.
        store
            .insert_symbol(&make_symbol("s1", "build", "r1"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s2", "build", "r2"))
            .unwrap();

        let links = super::discover_symbol_level_links(&store).unwrap();
        assert!(links.is_empty(), "short name 'build' should be skipped");
    }

    #[test]
    fn discover_symbol_level_links_returns_empty_for_single_repo() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/example/alpha"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("s1", "AuthService", "r1"))
            .unwrap();

        let links = super::discover_symbol_level_links(&store).unwrap();
        assert!(links.is_empty(), "single repo should produce no links");
    }

    #[test]
    fn suggest_links_detects_manifest_dependency() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&make_repo("r1", "https://github.com/myorg/app"))
            .unwrap();
        store
            .insert_repo(&make_repo("r2", "https://github.com/myorg/shared-types"))
            .unwrap();

        let mut manifests = HashMap::new();
        manifests.insert(
            "r1".to_string(),
            ManifestInfo {
                package_name: Some("@myorg/app".to_string()),
                dependencies: vec!["@myorg/shared-types".to_string(), "react".to_string()],
                entry_files: vec![],
            },
        );
        manifests.insert(
            "r2".to_string(),
            ManifestInfo {
                package_name: Some("@myorg/shared-types".to_string()),
                dependencies: vec![],
                entry_files: vec![],
            },
        );

        let suggestions = suggest_links(&store, &manifests).unwrap();
        let manifest_link = suggestions
            .links
            .iter()
            .find(|l| l.link_type == "package-dependency");
        assert!(manifest_link.is_some(), "should detect manifest dependency");
        let link = manifest_link.unwrap();
        assert_eq!(link.from, "app");
        assert_eq!(link.to, "shared-types");
        assert!(
            matches!(link.confidence, Confidence::High),
            "manifest link should be High confidence"
        );
    }
}

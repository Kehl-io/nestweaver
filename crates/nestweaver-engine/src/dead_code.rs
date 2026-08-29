//! Dead code detection via entry point reachability analysis.
//!
//! Walks forward from every entry point in the graph following
//! CALLS, IMPORTS, EXTENDS, IMPLEMENTS, and MEMBER_OF edges. Any
//! symbol not reached is potentially dead. Confidence scoring
//! accounts for visibility: private unreachable symbols are
//! high-confidence dead code; public ones could be library API.

use std::collections::{HashMap, HashSet, VecDeque};

use nestweaver_schema::SymbolKind;
use nestweaver_store::GraphStore;
use serde::Serialize;

use crate::manifest::ManifestInfo;

/// Confidence that a symbol is truly dead code.
///
/// Serialised lowercase to match [`Display`](std::fmt::Display), the daemon's
/// own payload, and the `--min-confidence` values a caller passes in. The
/// derived representation was the PascalCase variant name, so `dead-code --json`
/// answered "Medium" direct and "medium" through the daemon for the same run —
/// the same field disagreeing with itself depending on whether a daemon happened
/// to be running (nw-117).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeadCodeConfidence {
    /// Explicitly public symbol — could be consumed by code this graph does
    /// not contain (a library API, a re-export, a plugin surface).
    Low,
    /// The default tier. An unreachable symbol with no naming signal, whatever
    /// its visibility. Explicitly `private` lands HERE, not in [`Self::High`] —
    /// see [`infer_confidence`] for why.
    Medium,
    /// Unreachable AND spelled private-by-convention (leading `_`, or a
    /// lowercase-initial name in a Go file, which is unexported at the
    /// language level). This is the only signal that has ever discriminated
    /// on a real index.
    High,
}

impl DeadCodeConfidence {
    /// Parse from a CLI flag string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeadCodeConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

/// A symbol that was not reached from any entry point.
#[derive(Debug, Clone, Serialize)]
pub struct UnreachableSymbol {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub visibility: String,
    pub confidence: DeadCodeConfidence,
}

/// Summary result of the dead code detection pass.
#[derive(Debug, Clone, Serialize)]
pub struct DeadCodeResult {
    pub unreachable_symbols: Vec<UnreachableSymbol>,
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub dead_percentage: f64,
    /// Number of symbols excluded from analysis (type-only symbols, `.d.ts`
    /// declarations, properties, module declarations). These are not counted
    /// in `total_symbols`.
    pub excluded_count: usize,
}

/// Returns `true` for symbols that should be excluded from dead code analysis
/// because they are type-only constructs (erased at compile/runtime), live in
/// `.d.ts` declaration files, are properties (often accessed dynamically), or
/// are module declarations (e.g. Rust `pub mod alpha;` — a module is an
/// organizational declaration, never *called* from an entry point, yet it IS
/// the crate's public API surface, so reporting it as unreachable is pure
/// noise).
///
/// `SymbolKind` was surveyed for other non-callable declaration kinds:
/// `TypeAlias`/`Interface`/`Property` were already excluded; `Module` is the
/// only remaining declaration kind that cannot be reached by a call edge.
/// `Extension` is a member container analyzed like `Class` (its methods are
/// `Method` symbols); `Trait`/`Enum`/`Constant`/`Variable` are referenceable
/// items that can legitimately be dead, so they stay in the analysis.
fn is_excluded_from_dead_code(sym: &nestweaver_schema::Symbol) -> bool {
    matches!(
        sym.kind,
        SymbolKind::TypeAlias | SymbolKind::Interface | SymbolKind::Property | SymbolKind::Module
    ) || sym.file_path.ends_with(".d.ts")
}

/// UIDs of `Constant`/`Variable` symbols whose source span lies inside a
/// `Function` or `Method` in the same file — i.e. local bindings, not
/// declarations.
///
/// A local binding is not dead code and cannot be: it has no name anything
/// outside its enclosing body could use, so "is it reachable from an entry
/// point" is not a question that has an answer about it. Reporting one invites
/// a caller to go delete a line that a function two lines down is reading.
///
/// This is the `.tsx` half of nw-291's real-index evidence. React's
/// `const [activeView, setActiveView] = useState(..)` is captured as
/// `definition.const` by `typescript.scm`, so 173 of the first measured 1,000
/// candidates were component-local `const`s — `selected`, `badge`, `busy`,
/// `isZen`, `hideDetail`. Once the Rust false positives were fixed they floated
/// to 470 of 1,000, i.e. fixing everything else made this the dominant defect.
///
/// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? Everywhere, which is why the
/// test is on the SPAN rather than on the language: a `let` in a Go function, a
/// `const` in a Rust `fn` body, a module-level name assigned inside a Python
/// `def` — all have the same shape, and a per-language rule would have to be
/// written 32 times and would still miss the 33rd. The containing kind is
/// restricted to `Function`/`Method` on purpose: a `Class`, `Extension` or Rust
/// `impl` block also contains its members' spans, and an associated constant
/// IS externally addressable, so it stays in the analysis.
///
/// The fix deliberately lives here and not in `typescript.scm`. Dropping the
/// symbol at parse time would also drop it from search, `repo-map`, PageRank
/// and every UID that references it — a far larger blast radius than the one
/// defect being fixed, and those consumers WANT a local binding to be findable.
fn function_local_bindings(symbols: &[nestweaver_schema::Symbol]) -> HashSet<&str> {
    let mut by_file: HashMap<&str, Vec<&nestweaver_schema::Symbol>> = HashMap::new();
    for sym in symbols {
        if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
            || matches!(sym.kind, SymbolKind::Constant | SymbolKind::Variable)
        {
            by_file.entry(sym.file_path.as_str()).or_default().push(sym);
        }
    }

    let mut local = HashSet::new();
    for candidates in by_file.values() {
        let bodies: Vec<&nestweaver_schema::Symbol> = candidates
            .iter()
            .copied()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .collect();
        if bodies.is_empty() {
            continue;
        }
        for sym in candidates {
            if !matches!(sym.kind, SymbolKind::Constant | SymbolKind::Variable) {
                continue;
            }
            // A zero-width or inverted span cannot be reasoned about; leave it in.
            if sym.end_line < sym.start_line {
                continue;
            }
            if bodies
                .iter()
                .any(|body| body.start_line < sym.start_line && sym.end_line <= body.end_line)
            {
                local.insert(sym.uid.as_str());
            }
        }
    }
    local
}

/// Default minimum edge confidence for BFS traversal.
const DEFAULT_MIN_EDGE_CONFIDENCE: f32 = 0.3;

/// Edge confidence below which reachability is considered "weak".
/// Symbols reachable *only* via edges below this threshold are
/// reported as Medium confidence dead code rather than alive.
const WEAK_EDGE_THRESHOLD: f32 = 0.5;

/// Detect potentially dead code by walking forward from all entry points.
///
/// Algorithm:
/// 1. Collect every Symbol with `is_entry_point == true`.
/// 2. BFS forward from entry points following reachability edges,
///    skipping edges with confidence below `min_edge_confidence`.
/// 3. Symbols reachable only through low-confidence edges (< 0.5)
///    are reported as Medium confidence dead code instead of alive.
/// 4. Any symbol not in the visited set is reported as unreachable.
/// 5. Confidence is scored based on inferred visibility heuristics.
/// 6. Methods of dead classes are deduplicated (only the class is reported).
///
/// If `manifests` is provided, symbols whose file paths match `main`, `bin`,
/// or `exports` entries in a package.json manifest are treated as additional
/// entry points.
///
/// **Known limitation — reachability is only as complete as the edge set.**
/// A symbol is reported when no entry point reaches it, which is not the same
/// as "nothing references it". Two gaps are known and measured on a real Rust
/// index: references to a constant or static by bare name are only captured
/// for languages whose query file has a bare-identifier read rule, and a
/// symbol registered by a macro the parser treats as an opaque token tree is
/// reachable only if that macro is on the registration list in
/// `nestweaver-parser`. Treat EVERY tier as review candidates, not proof —
/// the caveat is not scoped to Low. Visibility IS persisted (it round-trips
/// through the store), but it deliberately does not promote a row to High;
/// see [`infer_confidence`].
///
/// **Performance note**: The BFS itself is O(V+E) and fast. On large graphs
/// (80K+ symbols, 100K+ edges), the dominant cost is loading all symbols and
/// typed edges from the database (~500-700ms). This is inherent to the full-
/// graph traversal approach and cannot be reduced without pre-computed caching.
pub fn detect_dead_code(store: &GraphStore) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, &HashMap::new(), None)
}

/// Like [`detect_dead_code`], but cooperatively bails when `cancel` trips (a
/// query timeout or client disconnect). The flag is checked once per BFS
/// dequeue; once tripped the walk returns a
/// [`nestweaver_store::StoreError::Cancelled`] (wrapped in `anyhow`, so the
/// boundary can downcast to distinguish cancellation from real failures) — a
/// cancelled walk is *incomplete*, never a legitimate (cacheable) result.
/// `cancel = None` never trips and is byte-for-byte the original behavior.
pub fn detect_dead_code_cancellable(
    store: &GraphStore,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, &HashMap::new(), cancel)
}

/// Like [`detect_dead_code`] but with an explicit minimum edge confidence
/// threshold. Edges with confidence below `min_edge_confidence` are not
/// traversed at all. Symbols reachable only via edges below
/// [`WEAK_EDGE_THRESHOLD`] (0.5) are reported as Medium confidence dead code.
pub fn detect_dead_code_with_confidence(
    store: &GraphStore,
    min_edge_confidence: f32,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, min_edge_confidence, &HashMap::new(), None)
}

/// Cancellable variant of [`detect_dead_code_with_confidence`]; see
/// [`detect_dead_code_cancellable`] for the cancellation contract.
pub fn detect_dead_code_with_confidence_cancellable(
    store: &GraphStore,
    min_edge_confidence: f32,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, min_edge_confidence, &HashMap::new(), cancel)
}

/// Like [`detect_dead_code`] but also accepts parsed manifest data so that
/// symbols in manifest entry files (`main`, `bin`, `exports`) are treated as
/// entry points.
pub fn detect_dead_code_with_manifests(
    store: &GraphStore,
    manifests: &HashMap<String, ManifestInfo>,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, manifests, None)
}

/// Cancellable variant of [`detect_dead_code_with_manifests`]; see
/// [`detect_dead_code_cancellable`] for the cancellation contract.
pub fn detect_dead_code_with_manifests_cancellable(
    store: &GraphStore,
    manifests: &HashMap<String, ManifestInfo>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, manifests, cancel)
}

/// Core implementation combining confidence-aware BFS with type exclusion,
/// manifest-driven entry points, and dead-class method deduplication.
fn detect_dead_code_inner(
    store: &GraphStore,
    min_edge_confidence: f32,
    manifests: &HashMap<String, ManifestInfo>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> anyhow::Result<DeadCodeResult> {
    // 1. Load all symbols and partition into analysable / excluded.
    let raw_symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    let function_local: HashSet<String> = function_local_bindings(&raw_symbols)
        .into_iter()
        .map(str::to_string)
        .collect();
    let excluded_count = raw_symbols
        .iter()
        .filter(|s| is_excluded_from_dead_code(s) || function_local.contains(&s.uid))
        .count();
    let all_symbols: Vec<_> = raw_symbols
        .into_iter()
        .filter(|s| !is_excluded_from_dead_code(s) && !function_local.contains(&s.uid))
        .collect();

    if all_symbols.is_empty() {
        return Ok(DeadCodeResult {
            unreachable_symbols: vec![],
            total_symbols: 0,
            reachable_symbols: 0,
            dead_percentage: 0.0,
            excluded_count,
        });
    }

    // 2. Load the full code graph (symbols + typed edges).
    let typed_edges = store
        .load_typed_edges()
        .map_err(|e| anyhow::anyhow!("load_typed_edges: {e}"))?;

    // Build adjacency list: source -> [(target, confidence)].
    // Also add reverse MEMBER_OF edges (class -> member) so that when BFS
    // reaches a class, its members become reachable too.
    // Additionally, track class -> [member_uid] for dedup in step 6.
    let mut adjacency: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    let mut class_members: HashMap<String, Vec<String>> = HashMap::new();
    for (src, dst, edge_type, confidence, _evidence) in &typed_edges {
        let conf = *confidence as f32;
        adjacency
            .entry(src.clone())
            .or_default()
            .push((dst.clone(), conf));
        if edge_type == "MEMBER_OF" {
            // MEMBER_OF goes member->class; reverse it so class->member is also traversed.
            adjacency
                .entry(dst.clone())
                .or_default()
                .push((src.clone(), conf));
            // Track class -> [members] for dead-class dedup.
            class_members
                .entry(dst.clone())
                .or_default()
                .push(src.clone());
        }
    }

    // 3. Collect manifest entry file paths (normalized, no leading `./`).
    let manifest_entry_files: HashSet<String> = manifests
        .values()
        .flat_map(|m| m.entry_files.iter())
        .map(|p| p.strip_prefix("./").unwrap_or(p).to_string())
        .collect();

    // 4. Identify entry points (flag + manifest-driven).
    let mut entry_point_uids: Vec<String> = Vec::new();
    for sym in &all_symbols {
        if sym.is_entry_point {
            entry_point_uids.push(sym.uid.clone());
            continue;
        }
        // Manifest-driven: exported symbols in manifest entry files.
        if !manifest_entry_files.is_empty() {
            let normalized = sym.file_path.strip_prefix("./").unwrap_or(&sym.file_path);
            if manifest_entry_files.contains(normalized) {
                entry_point_uids.push(sym.uid.clone());
            }
        }
    }

    // 5. Confidence-aware BFS from all entry points.
    //
    // Two-pass BFS:
    //   - `strong_visited`: symbols reachable via at least one path where
    //     every edge has confidence >= WEAK_EDGE_THRESHOLD.
    //   - `weak_visited`: symbols reachable via edges above min_edge_confidence
    //     but NOT via any fully-strong path.
    //
    // We track the "max minimum confidence along any path" for each node.
    // If max_min >= WEAK_EDGE_THRESHOLD the symbol is strongly reachable;
    // otherwise it's weakly reachable.
    let mut best_path_conf: HashMap<String, f32> = HashMap::new();
    let mut queue: VecDeque<(String, f32)> = VecDeque::new();

    for uid in &entry_point_uids {
        let prev = best_path_conf.entry(uid.clone()).or_insert(0.0_f32);
        if *prev < 1.0 {
            *prev = 1.0; // entry points themselves are fully confident
            queue.push_back((uid.clone(), 1.0));
        }
    }

    while let Some((current, path_conf)) = queue.pop_front() {
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            // Propagate the store's typed cancellation error through anyhow so
            // the tool boundary can downcast and distinguish "cancelled" from
            // a real failure. A cancelled walk is incomplete, never cacheable.
            return Err(anyhow::Error::new(nestweaver_store::StoreError::Cancelled(
                nestweaver_store::CancelReason::Timeout,
            )));
        }
        if let Some(targets) = adjacency.get(&current) {
            for (target, edge_conf) in targets {
                // Skip edges below the minimum confidence threshold entirely.
                if *edge_conf < min_edge_confidence {
                    continue;
                }

                // The path confidence is the minimum confidence along
                // the entire path from an entry point to this target.
                let new_path_conf = path_conf.min(*edge_conf);

                let entry = best_path_conf.entry(target.clone()).or_insert(0.0_f32);
                if new_path_conf > *entry {
                    *entry = new_path_conf;
                    queue.push_back((target.clone(), new_path_conf));
                }
            }
        }
    }

    // 6. Collect unreachable symbols with confidence scoring.
    let total_symbols = all_symbols.len();

    // Symbols in best_path_conf with strong path confidence are truly reachable.
    let strong_reachable: HashSet<&String> = best_path_conf
        .iter()
        .filter(|(_, conf)| **conf >= WEAK_EDGE_THRESHOLD)
        .map(|(uid, _)| uid)
        .collect();

    // Symbols reachable only via weak paths.
    let weak_reachable: HashSet<&String> = best_path_conf
        .iter()
        .filter(|(uid, conf)| **conf < WEAK_EDGE_THRESHOLD && !strong_reachable.contains(uid))
        .map(|(uid, _)| uid)
        .collect();

    // Build a lookup: uid -> kind for dead-class dedup.
    let kind_by_uid: HashMap<&str, SymbolKind> = all_symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.kind))
        .collect();

    // Find unreachable class UIDs so we can suppress their members.
    let unreachable_class_uids: HashSet<&str> = all_symbols
        .iter()
        .filter(|s| !strong_reachable.contains(&s.uid) && s.kind == SymbolKind::Class)
        .map(|s| s.uid.as_str())
        .collect();

    // Collect member UIDs of dead classes (to suppress from the unreachable list).
    let suppressed_member_uids: HashSet<String> = unreachable_class_uids
        .iter()
        .flat_map(|cls_uid| class_members.get(*cls_uid).cloned().unwrap_or_default())
        .filter(|member_uid| {
            // Only suppress if the member is actually a Method and is also unreachable.
            kind_by_uid.get(member_uid.as_str()) == Some(&SymbolKind::Method)
                && !strong_reachable.contains(member_uid)
        })
        .collect();

    // nw-291 (M5): carried alongside each row purely for ordering. `--limit N`
    // takes the PREFIX of this order, so with no importance term it was
    // "the first N alphabetically by path" — 726 of 1000 reported rows came
    // from a single repo, stopping mid-`r`. PageRank is already loaded onto
    // every symbol; it was simply never consulted.
    let mut ranked: Vec<(f64, UnreachableSymbol)> = Vec::new();
    for sym in &all_symbols {
        if strong_reachable.contains(&sym.uid) {
            continue;
        }
        // Suppress methods of dead classes — the class itself is reported.
        if suppressed_member_uids.contains(&sym.uid) {
            continue;
        }

        let visibility_str = sym.visibility.to_string();

        // Symbols reachable only via weak edges are reported as Medium
        // confidence dead code — they might be reachable but the edges
        // are not highly confident.
        let confidence = if weak_reachable.contains(&sym.uid) {
            DeadCodeConfidence::Medium
        } else {
            infer_confidence(&sym.name, &visibility_str, &sym.file_path)
        };

        ranked.push((
            sym.pagerank_score.unwrap_or(0.0),
            UnreachableSymbol {
                uid: sym.uid.clone(),
                name: sym.name.clone(),
                kind: sym.kind.to_string(),
                file_path: sym.file_path.clone(),
                visibility: visibility_str,
                confidence,
            },
        ));
    }

    // Sort by confidence descending, then by IMPORTANCE descending, then by
    // file path and name for a total, deterministic order. The path/name tail
    // is kept so equal-importance rows still sort stably; it is no longer the
    // primary discriminator.
    ranked.sort_by(|(a_rank, a), (b_rank, b)| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| {
                b_rank
                    .partial_cmp(a_rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.name.cmp(&b.name))
    });
    let unreachable_symbols: Vec<UnreachableSymbol> =
        ranked.into_iter().map(|(_, sym)| sym).collect();

    let reachable_symbols = total_symbols - unreachable_symbols.len();

    let dead_percentage = if total_symbols > 0 {
        (unreachable_symbols.len() as f64 / total_symbols as f64) * 100.0
    } else {
        0.0
    };

    Ok(DeadCodeResult {
        unreachable_symbols,
        total_symbols,
        reachable_symbols,
        dead_percentage,
        excluded_count,
    })
}

/// Infer dead-code confidence from visibility and naming conventions.
///
/// - Explicitly public -> Low (could be library API); this wins over every
///   naming convention below (nw-155).
/// - Names starting with `_`, or lowercase-initial in Go files -> High
///   (private by convention AND unaddressable from outside the file/package).
/// - Everything else, INCLUDING an explicitly private/internal/protected
///   symbol -> Medium.
///
/// # Why `private` alone does not reach High (nw-291 follow-up)
///
/// Before visibility was persisted, symbol reads rebuilt every row as
/// `Inferred`, so the `visibility == "private"` guard here was unreachable and
/// this population scored Medium. Persisting the column made the guard live —
/// and promoted the same rows to High without anything having validated that
/// the rows were right. On a fresh Rust index the resulting High tier was
/// 1000/1000 `private`, and its top 15 contained zero true positives: all were
/// `criterion_group!`-registered benchmark functions or file-local constants
/// whose references the graph does not yet carry.
///
/// The command's own help scopes its caveats to LOW-confidence results, so
/// promoting that population to High would present the least trustworthy list
/// as the most trustworthy one — strictly worse for a caller than the Medium
/// it used to get. `private` is a NECESSARY condition for "cannot be called
/// from outside", not a SUFFICIENT one for "is dead": it says nothing about
/// whether the in-file references were captured at all.
///
/// So visibility keeps demoting (public -> Low, which is safe: it moves rows
/// AWAY from the trustworthy tier) and no longer promotes. High is reserved
/// for the naming conventions that were the live discriminator before this
/// column existed, so this branch is not worse-calibrated than the tier it
/// replaces. Re-couple `private` to High only with a measured precision number
/// on a real index behind it.
fn infer_confidence(name: &str, visibility: &str, file_path: &str) -> DeadCodeConfidence {
    // Explicitly public wins over every naming convention below (nw-155).
    //
    // A leading underscore means "private by convention", but an explicit
    // export overrides the convention: `export { __wbg_init as default }`
    // makes that symbol a module's PUBLIC entry point no matter how it is
    // spelled. Reporting it as high-confidence dead code invites a user to
    // delete live, exported API.
    if visibility == "public" {
        return DeadCodeConfidence::Low;
    }

    // Naming-convention heuristics for private scope:
    //   - Leading underscore (Python, JS/TS, Dart, Ruby)
    //   - Lowercase first char in Go files (unexported)
    if name.starts_with('_') {
        return DeadCodeConfidence::High;
    }
    let is_go = file_path.ends_with(".go");
    if is_go && name.chars().next().is_some_and(|c| c.is_lowercase()) {
        return DeadCodeConfidence::High;
    }

    // Everything else — including an explicitly private/internal/protected
    // symbol with no naming signal — is Medium. See the note above.
    DeadCodeConfidence::Medium
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    fn make_symbol(uid: &str, name: &str, is_entry: bool) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: is_entry,
            entry_point_kind: if is_entry {
                Some(nestweaver_schema::EntryPointKind::Main)
            } else {
                None
            },
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn make_symbol_with_kind(
        uid: &str,
        name: &str,
        kind: SymbolKind,
        file_path: &str,
        is_entry: bool,
    ) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind,
            repo_uid: "repo-1".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: is_entry,
            entry_point_kind: if is_entry {
                Some(nestweaver_schema::EntryPointKind::Main)
            } else {
                None
            },
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    #[test]
    fn empty_graph_returns_empty_result() {
        let store = GraphStore::in_memory().unwrap();
        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 0);
        assert_eq!(result.reachable_symbols, 0);
        assert!(result.unreachable_symbols.is_empty());
        assert_eq!(result.excluded_count, 0);
    }

    /// A pre-tripped cancel flag must make the reachability BFS return the
    /// store's `Cancelled` error (downcastable through anyhow) on its first
    /// dequeue — never a (truncated) Ok result.
    #[test]
    fn detect_dead_code_cancellable_bails_when_flag_is_set() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let store = GraphStore::in_memory().unwrap();
        // An entry point guarantees the BFS queue is non-empty, so the
        // per-dequeue cancel check actually runs.
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();

        let cancel = Arc::new(AtomicBool::new(true));
        let err = detect_dead_code_cancellable(&store, Some(&cancel))
            .expect_err("pre-cancelled dead-code walk must return Err");
        let store_err = err
            .downcast_ref::<nestweaver_store::StoreError>()
            .expect("boundary must be able to downcast to StoreError");
        assert!(
            store_err.is_cancelled(),
            "expected StoreError::Cancelled, got: {store_err}"
        );

        // Untripped flag: byte-for-byte the original behavior.
        let untripped = Arc::new(AtomicBool::new(false));
        assert!(detect_dead_code_cancellable(&store, Some(&untripped)).is_ok());
    }

    #[test]
    fn all_reachable_from_entry_point() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> a -> b
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b", "fn_b", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "a".to_string(),
                target_uid: "b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
        assert_eq!(result.dead_percentage, 0.0);
    }

    #[test]
    fn detects_unreachable_symbol() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> a, but orphan is disconnected
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("orphan", "orphan_fn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 2);
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "orphan_fn");
    }

    #[test]
    fn follows_imports_and_extends_edges() {
        let store = GraphStore::in_memory().unwrap();

        // entry --imports--> imported --extends--> base
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("imported", "Imported", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("base", "Base", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "imported".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "imported".to_string(),
                target_uid: "base".to_string(),
                edge_type: EdgeType::Extends,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn no_entry_points_marks_everything_unreachable() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b", "fn_b", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "a".to_string(),
                target_uid: "b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 2);
        assert_eq!(result.reachable_symbols, 0);
        assert_eq!(result.unreachable_symbols.len(), 2);
    }

    #[test]
    fn imports_chain_reaches_transitive_deps_and_detects_dead_private() {
        let store = GraphStore::in_memory().unwrap();

        // Build a realistic multi-module graph:
        //   entry (entry point) --IMPORTS--> moduleB_pub
        //   moduleB_pub --CALLS--> moduleC_util
        //   moduleC_dead (private, no incoming edges) <- truly dead
        let mut entry = make_symbol("entry", "App", true);
        entry.file_path = "src/app.tsx".to_string();
        store.insert_symbol(&entry).unwrap();

        let mut module_b = make_symbol("moduleB_pub", "formatDate", false);
        module_b.file_path = "src/utils/date.ts".to_string();
        store.insert_symbol(&module_b).unwrap();

        let mut module_c = make_symbol("moduleC_util", "parseISO", false);
        module_c.file_path = "src/utils/parse.ts".to_string();
        store.insert_symbol(&module_c).unwrap();

        let mut dead_fn = make_symbol("moduleC_dead", "_unusedHelper", false);
        dead_fn.file_path = "src/utils/parse.ts".to_string();
        dead_fn.visibility = Visibility::Private;
        store.insert_symbol(&dead_fn).unwrap();

        // entry --IMPORTS--> moduleB_pub
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "moduleB_pub".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // moduleB_pub --CALLS--> moduleC_util
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "moduleB_pub".to_string(),
                target_uid: "moduleC_util".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();

        // entry, moduleB_pub, moduleC_util should all be reachable
        assert_eq!(result.total_symbols, 4);
        assert_eq!(result.reachable_symbols, 3);

        // Only the private unused helper is dead
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "_unusedHelper");
        assert_eq!(
            result.unreachable_symbols[0].confidence,
            DeadCodeConfidence::High
        );
    }

    #[test]
    fn member_of_reverse_traversal_reaches_class_members() {
        let store = GraphStore::in_memory().unwrap();

        // entry --IMPORTS--> MyClass
        // method --MEMBER_OF--> MyClass  (BFS should reverse this to reach method)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("cls", "MyClass", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("method", "doWork", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "method".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    /// nw-155: an explicit export outranks the underscore convention. All 154
    /// high-confidence results on the reference graph began with `_`, and among
    /// them were `__wbg_init` -- a module's DEFAULT EXPORT -- plus three
    /// functions called from within the same file.
    #[test]
    fn an_exported_underscore_symbol_is_not_high_confidence() {
        assert_eq!(
            infer_confidence("__wbg_init", "public", "src/wasm/glue.js"),
            DeadCodeConfidence::Low,
            "an exported symbol must not be high-confidence dead code however it is spelled"
        );
        // The convention still applies where nothing contradicts it.
        assert_eq!(
            infer_confidence("_helper", "inferred", "src/lib.py"),
            DeadCodeConfidence::High
        );
    }

    /// nw-291 / F-DC-2: the nw-155 guard above is asserted at unit level on an
    /// input the pipeline cannot produce — `read.rs` rebuilt every symbol's
    /// visibility as `Inferred`, so BOTH `public` guards and the
    /// private/internal/protected guard in `infer_confidence` were unreachable
    /// and the only live discriminator was `name.starts_with('_')`. Assert END
    /// TO END, through the store.
    #[test]
    fn exported_underscore_symbol_is_not_high_confidence_through_the_store() {
        let store = GraphStore::in_memory().unwrap();
        let mut sym = make_symbol_with_kind(
            "wbg",
            "__wbg_init",
            SymbolKind::Function,
            "src/wasm/glue.js",
            false,
        );
        sym.visibility = Visibility::Public;
        store.insert_symbol(&sym).unwrap();

        let result = detect_dead_code(&store).unwrap();
        let row = result
            .unreachable_symbols
            .iter()
            .find(|s| s.name == "__wbg_init")
            .expect("symbol is unreachable and must be reported");

        assert_ne!(
            row.confidence,
            DeadCodeConfidence::High,
            "an explicitly public symbol must not reach the high tier; got visibility={:?}",
            row.visibility
        );
        assert_eq!(
            row.visibility, "public",
            "visibility must survive a store round-trip"
        );
    }

    /// Where else does this property hold? The private/internal/protected guard
    /// is the same branch in the other direction. The column must still survive
    /// the round trip — other work depends on it — but it must NOT carry the
    /// row into the tier the command presents as trustworthy.
    ///
    /// nw-291 follow-up: this assertion used to demand `High`. On a fresh Rust
    /// index that promotion produced a top-1000 that was 1000/1000 `private`
    /// and 0/15 true positives at the top, i.e. a ~0%-precision list served as
    /// the HIGH tier while the command's help scopes its caveats to LOW. Before
    /// visibility was persisted this same population came out `Medium`, so the
    /// promotion was a regression against `main`, not merely an unfixed bug.
    #[test]
    fn explicit_private_visibility_survives_the_store_round_trip() {
        let store = GraphStore::in_memory().unwrap();
        let mut sym =
            make_symbol_with_kind("p", "Helper", SymbolKind::Function, "src/lib.ts", false);
        sym.visibility = Visibility::Private;
        store.insert_symbol(&sym).unwrap();

        let result = detect_dead_code(&store).unwrap();
        let row = result
            .unreachable_symbols
            .iter()
            .find(|s| s.name == "Helper")
            .expect("Helper must be reported");
        assert_eq!(
            row.visibility, "private",
            "the column is correct and must keep round-tripping"
        );
        assert_eq!(
            row.confidence,
            DeadCodeConfidence::Medium,
            "`private` alone must not reach the tier the help calls trustworthy"
        );
    }

    /// nw-291 follow-up, the load-bearing guard: whatever else changes about
    /// the tiers, an unreachable row must never be promoted to `High` on the
    /// strength of `visibility` alone. Asserted over the whole `Visibility`
    /// enum so a variant added later cannot quietly re-open the promotion —
    /// the name carries no private-by-convention signal, so the ONLY thing
    /// that could lift it is visibility.
    #[test]
    fn no_visibility_alone_promotes_a_row_to_high() {
        for visibility in [
            Visibility::Public,
            Visibility::Private,
            Visibility::Protected,
            Visibility::Internal,
            Visibility::Inferred,
        ] {
            let store = GraphStore::in_memory().unwrap();
            let mut sym =
                make_symbol_with_kind("v", "Helper", SymbolKind::Function, "src/lib.ts", false);
            sym.visibility = visibility;
            store.insert_symbol(&sym).unwrap();

            let result = detect_dead_code(&store).unwrap();
            let row = result
                .unreachable_symbols
                .iter()
                .find(|s| s.name == "Helper")
                .expect("Helper must be reported");
            assert_ne!(
                row.confidence,
                DeadCodeConfidence::High,
                "visibility={visibility:?} must not reach High on its own"
            );
        }
    }

    /// nw-291 follow-up / F-DC evidence. `typescript.scm` captures
    /// `const [activeView, setActiveView] = useState(..)` as `definition.const`,
    /// so React component-local bindings became dead-code candidates: 173 of the
    /// first measured 1,000, and 470 of 1,000 once the Rust false positives were
    /// fixed and they floated up. A local binding has no name anything outside
    /// its enclosing body could use, so "is it reachable from an entry point" is
    /// not a question that has an answer about it.
    #[test]
    fn a_binding_inside_a_function_body_is_not_a_dead_code_candidate() {
        let store = GraphStore::in_memory().unwrap();
        let mut component = make_symbol_with_kind(
            "c",
            "AppContent",
            SymbolKind::Function,
            "src/App.tsx",
            false,
        );
        component.start_line = 10;
        component.end_line = 90;
        let mut local = make_symbol_with_kind(
            "l",
            "activeView",
            SymbolKind::Constant,
            "src/App.tsx",
            false,
        );
        local.start_line = 20;
        local.end_line = 20;
        let mut module_level = make_symbol_with_kind(
            "m",
            "DEFAULT_VIEW",
            SymbolKind::Constant,
            "src/App.tsx",
            false,
        );
        module_level.start_line = 3;
        module_level.end_line = 3;
        for sym in [&component, &local, &module_level] {
            store.insert_symbol(sym).unwrap();
        }

        let result = detect_dead_code(&store).unwrap();
        let reported: Vec<&str> = result
            .unreachable_symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !reported.contains(&"activeView"),
            "a component-local binding was reported as dead code: {reported:?}"
        );
        assert!(
            reported.contains(&"DEFAULT_VIEW"),
            "a MODULE-level constant is externally addressable and must stay in \
             the analysis: {reported:?}"
        );
        assert!(
            reported.contains(&"AppContent"),
            "the enclosing function is itself a candidate: {reported:?}"
        );
        assert_eq!(
            result.excluded_count, 1,
            "the excluded binding must be counted as excluded, not silently \
             dropped from the totals"
        );
    }

    /// WHERE ELSE: the rule is keyed on SPANS, not on a language, so it must
    /// NOT fire for a member container. A Rust `impl` block and a TS class both
    /// contain their members' spans, and an associated constant IS externally
    /// addressable.
    #[test]
    fn an_associated_constant_inside_a_class_stays_in_the_analysis() {
        let store = GraphStore::in_memory().unwrap();
        let mut class =
            make_symbol_with_kind("k", "Config", SymbolKind::Class, "src/config.rs", false);
        class.start_line = 1;
        class.end_line = 40;
        let mut assoc = make_symbol_with_kind(
            "a",
            "MAX_DEPTH",
            SymbolKind::Constant,
            "src/config.rs",
            false,
        );
        assoc.start_line = 5;
        assoc.end_line = 5;
        store.insert_symbol(&class).unwrap();
        store.insert_symbol(&assoc).unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert!(
            result
                .unreachable_symbols
                .iter()
                .any(|s| s.name == "MAX_DEPTH"),
            "an associated constant was excluded as if it were a local binding"
        );
    }

    /// nw-291 / F-DC-6: `--limit N` took the first N of a
    /// (confidence, file_path, name) ordering, i.e. an ALPHABETICAL prefix —
    /// 726 of 1000 rows came from one repo, stopping mid-`r`. There is no
    /// importance term even though PageRank is loaded onto every symbol.
    #[test]
    fn limit_order_is_by_importance_not_alphabetical_path() {
        let store = GraphStore::in_memory().unwrap();
        let mut trivial = make_symbol_with_kind(
            "t",
            "Trivial",
            SymbolKind::Function,
            "aaa/trivial.rs",
            false,
        );
        trivial.pagerank_score = Some(0.001);
        let mut important =
            make_symbol_with_kind("i", "Important", SymbolKind::Function, "zzz/core.rs", false);
        important.pagerank_score = Some(0.9);
        store.insert_symbol(&trivial).unwrap();
        store.insert_symbol(&important).unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.unreachable_symbols[0].name, "Important");
    }

    #[test]
    fn confidence_scoring_private_names() {
        // Leading underscore -> High
        assert_eq!(
            infer_confidence("_helper", "inferred", "src/lib.py"),
            DeadCodeConfidence::High
        );
        // Go lowercase -> High
        assert_eq!(
            infer_confidence("helper", "inferred", "pkg/utils.go"),
            DeadCodeConfidence::High
        );
        // Public -> Low
        assert_eq!(
            infer_confidence("Helper", "public", "src/lib.rs"),
            DeadCodeConfidence::Low
        );
        // Inferred, no private signal -> Medium
        assert_eq!(
            infer_confidence("Helper", "inferred", "src/lib.rs"),
            DeadCodeConfidence::Medium
        );
        // Explicit private with no naming signal -> Medium, NOT High. See
        // `infer_confidence`: the promotion this used to assert was measured
        // at 0/15 true positives on a real Rust index.
        assert_eq!(
            infer_confidence("Helper", "private", "src/lib.rs"),
            DeadCodeConfidence::Medium
        );
        assert_eq!(
            infer_confidence("Helper", "internal", "src/lib.cs"),
            DeadCodeConfidence::Medium
        );
        assert_eq!(
            infer_confidence("Helper", "protected", "src/lib.java"),
            DeadCodeConfidence::Medium
        );
        // Public still outranks the underscore convention (nw-155): the
        // demotion direction is unchanged.
        assert_eq!(
            infer_confidence("__wbg_init", "public", "src/wasm/glue.js"),
            DeadCodeConfidence::Low
        );
    }

    // ── Confidence-aware BFS tests ──

    #[test]
    fn low_confidence_edges_are_skipped_below_threshold() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.2--> weak_target (below default 0.3 threshold)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("weak_target", "weakFn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "weak_target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.2,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // weak_target should NOT be reachable (edge below 0.3)
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "weakFn");
    }

    #[test]
    fn weak_edges_produce_medium_confidence_dead_code() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.4--> borderline (above 0.3 min, below 0.5 weak threshold)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("borderline", "maybeDead", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "borderline".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.4,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // borderline should be reported as Medium confidence dead code
        // because it's only reachable via a weak edge (0.4 < 0.5)
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "maybeDead");
        assert_eq!(
            result.unreachable_symbols[0].confidence,
            DeadCodeConfidence::Medium
        );
    }

    #[test]
    fn strong_edges_still_mark_symbols_as_reachable() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.9--> strong_target (well above both thresholds)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("strong_target", "strongFn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "strong_target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.reachable_symbols, 2);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn custom_min_confidence_threshold() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.5--> target
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("target", "fn_a", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.5,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // With min_confidence=0.6, the edge (0.5) should be skipped entirely
        let result = detect_dead_code_with_confidence(&store, 0.6).unwrap();
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "fn_a");

        // With min_confidence=0.3 (default), the edge (0.5) should be traversed
        // and 0.5 >= 0.5 weak threshold, so strongly reachable
        let result = detect_dead_code_with_confidence(&store, 0.3).unwrap();
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn mixed_strong_and_weak_paths_uses_best() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.4--> target (weak path)
        // entry --0.9--> middle --0.8--> target (strong path)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("middle", "helper", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("target", "fn_a", false))
            .unwrap();

        // Weak direct path
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.4,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // Strong indirect path
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "middle".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "middle".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.8,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // target should be strongly reachable via the strong path (min 0.8 >= 0.5)
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    // ── Type exclusion tests ──

    #[test]
    fn type_alias_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "alias",
                "MyType",
                SymbolKind::TypeAlias,
                "src/types.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        // TypeAlias should not appear in total_symbols or unreachable.
        assert_eq!(result.total_symbols, 1);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn interface_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "iface",
                "IUser",
                SymbolKind::Interface,
                "src/types.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    #[test]
    fn property_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "prop",
                "name",
                SymbolKind::Property,
                "src/model.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    #[test]
    fn d_ts_symbols_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        // A function in a .d.ts file should be excluded.
        store
            .insert_symbol(&make_symbol_with_kind(
                "decl",
                "fetchData",
                SymbolKind::Function,
                "src/api.d.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    #[test]
    fn module_excluded_from_dead_code() {
        // Rust `pub mod alpha;` produces a Module symbol that no entry point
        // ever *calls* — it is the crate's public API surface, not dead code.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "mod-alpha",
                "alpha",
                SymbolKind::Module,
                "src/lib.rs",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
        assert!(
            result.unreachable_symbols.is_empty(),
            "module declarations must not be reported as unreachable: {:?}",
            result.unreachable_symbols
        );
    }

    // ── Dead class method dedup tests ──

    #[test]
    fn dead_class_methods_not_double_counted() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();

        // Dead class with two methods.
        store
            .insert_symbol(&make_symbol_with_kind(
                "cls",
                "DeadClass",
                SymbolKind::Class,
                "src/dead.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m1",
                "methodA",
                SymbolKind::Method,
                "src/dead.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m2",
                "methodB",
                SymbolKind::Method,
                "src/dead.ts",
                false,
            ))
            .unwrap();

        // m1 --MEMBER_OF--> cls, m2 --MEMBER_OF--> cls
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m1".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m2".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // Only the class should be in unreachable, not its methods.
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "DeadClass");
        assert_eq!(result.unreachable_symbols[0].kind, "Class");
    }

    #[test]
    fn reachable_class_methods_not_suppressed() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> cls (reachable class), method is MEMBER_OF cls
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "cls",
                "LiveClass",
                SymbolKind::Class,
                "src/live.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m1",
                "methodA",
                SymbolKind::Method,
                "src/live.ts",
                false,
            ))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m1".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // Class is reachable, BFS reaches method via reverse MEMBER_OF.
        assert!(result.unreachable_symbols.is_empty());
    }

    // ── Manifest-driven entry point tests ──

    #[test]
    fn manifest_entry_files_mark_symbols_as_entry_points() {
        let store = GraphStore::in_memory().unwrap();

        // No explicit entry point, but the symbol's file is a manifest entry.
        store
            .insert_symbol(&make_symbol_with_kind(
                "lib",
                "libMain",
                SymbolKind::Function,
                "src/index.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "orphan",
                "orphanFn",
                SymbolKind::Function,
                "src/utils.ts",
                false,
            ))
            .unwrap();

        let mut manifests = HashMap::new();
        manifests.insert(
            "repo-1".to_string(),
            ManifestInfo {
                package_name: Some("my-pkg".to_string()),
                dependencies: vec![],
                entry_files: vec!["./src/index.ts".to_string()],
            },
        );

        let result = detect_dead_code_with_manifests(&store, &manifests).unwrap();
        assert_eq!(result.total_symbols, 2);
        // libMain should be reachable (manifest entry), orphanFn should not.
        assert_eq!(result.reachable_symbols, 1);
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "orphanFn");
    }

    #[test]
    fn manifest_entry_file_without_leading_dot_slash() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol_with_kind(
                "bin",
                "cliMain",
                SymbolKind::Function,
                "bin/cli.js",
                false,
            ))
            .unwrap();

        let mut manifests = HashMap::new();
        manifests.insert(
            "repo-1".to_string(),
            ManifestInfo {
                package_name: Some("my-cli".to_string()),
                dependencies: vec![],
                entry_files: vec!["bin/cli.js".to_string()],
            },
        );

        let result = detect_dead_code_with_manifests(&store, &manifests).unwrap();
        assert_eq!(result.reachable_symbols, 1);
        assert!(result.unreachable_symbols.is_empty());
    }
}

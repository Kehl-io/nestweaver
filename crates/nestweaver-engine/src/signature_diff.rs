//! Self-model API-signature diff engine.
//!
//! Classifies whether a public symbol changed in a *breaking* way by comparing
//! its indexed signature / `type_info` / visibility BEFORE vs AFTER, rather than
//! guessing from graph centrality. Everything here is pure and side-effect-free:
//! callers supply the two already-indexed [`Symbol`] snapshots (parsing the two
//! commits and matching symbols is a separate concern) and receive a structured
//! list of [`BreakingChange`]s.
//!
//! # What the schema lets us decide
//! [`TypeInfo::parameter_types`] is `Vec<(name, Option<type>)>`, where the
//! `Option` means the type may be *unresolved* (tree-sitter didn't recover it),
//! NOT that the parameter is optional. Optional-vs-required is therefore **not**
//! representable in this schema and is deliberately not attempted — see the
//! module-level notes in the crate docs / the follow-up backlog item.

use nestweaver_schema::{Symbol, TypeInfo, Visibility};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The category of a breaking change to a public symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreakKind {
    Removed,
    Renamed,
    VisibilityNarrowed,
    ParamAdded,
    ParamRemoved,
    ParamReordered,
    ParamRetyped,
    ReturnTypeChanged,
    DeclaredTypeChanged,
}

/// Confidence that the change is truly breaking. `Breaking` = decidable from
/// structure alone (all languages); `LikelyBreaking` = a type change where both
/// sides' types resolved; `ReachOnly` = a type change where a side is unresolved
/// (tree-sitter didn't recover the type) — surface as a hint, not a verified break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreakTier {
    Breaking,
    LikelyBreaking,
    ReachOnly,
}

/// A single classified breaking change to one public symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakingChange {
    pub symbol_uid: String,
    pub symbol_name: String,
    pub kind: BreakKind,
    pub tier: BreakTier,
    pub detail: String,
}

/// Whether a symbol's visibility is public-facing for API-break purposes.
///
/// `Public` is public; `Inferred` means the extractor couldn't determine
/// visibility, so we over-approximate and treat it as visible. Everything else
/// (`Internal` / `Protected` / `Private`) is not part of the public API.
fn is_public_facing(v: Visibility) -> bool {
    matches!(v, Visibility::Public | Visibility::Inferred)
}

/// Structural equality of two `Option<TypeInfo>`.
///
/// `TypeInfo` doesn't derive `PartialEq` in the schema crate, so we compare its
/// fields here to keep this module self-contained (no schema change required).
fn type_info_eq(a: &Option<TypeInfo>, b: &Option<TypeInfo>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.declared_type == y.declared_type
                && x.return_type == y.return_type
                && x.parameter_types == y.parameter_types
        }
        _ => false,
    }
}

/// Render an optional type for `detail` strings.
fn ty_str(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("<unresolved>")
}

/// Tier for a type change: both sides resolved → `LikelyBreaking`, otherwise
/// (a side is `None`/unresolved) → `ReachOnly`.
fn type_change_tier(before: &Option<String>, after: &Option<String>) -> BreakTier {
    if before.is_some() && after.is_some() {
        BreakTier::LikelyBreaking
    } else {
        BreakTier::ReachOnly
    }
}

/// Stable key for matching an `after` symbol to a `before` symbol: prefer the
/// instance-independent `canonical_id`, else `(name, file_path)`.
fn stable_key(s: &Symbol) -> String {
    match &s.canonical_id {
        Some(id) => id.clone(),
        None => format!("{}\u{0}{}", s.name, s.file_path),
    }
}

/// Rank used to make the sorted output of [`diff_public_api`] deterministic.
fn kind_rank(k: BreakKind) -> u8 {
    match k {
        BreakKind::Removed => 0,
        BreakKind::Renamed => 1,
        BreakKind::VisibilityNarrowed => 2,
        BreakKind::ParamAdded => 3,
        BreakKind::ParamRemoved => 4,
        BreakKind::ParamReordered => 5,
        BreakKind::ParamRetyped => 6,
        BreakKind::ReturnTypeChanged => 7,
        BreakKind::DeclaredTypeChanged => 8,
    }
}

/// Diff two snapshots of the *same logical symbol* (already matched) and return
/// the breaking changes between them.
///
/// The two symbols are assumed to be the same API surface at two points in time.
/// See the module docs for the classification rules.
pub fn diff_symbol(before: &Symbol, after: &Symbol) -> Vec<BreakingChange> {
    let mut out = Vec::new();

    // API gate: only public-facing `before` symbols can incur an API break.
    if !is_public_facing(before.visibility) {
        return out;
    }

    // Body-only filter: if the API surface (signature + type_info + visibility)
    // is unchanged, this is an implementation change — NOT an API break — even
    // if `content_hash` differs. Never flag on content_hash alone.
    if before.signature == after.signature
        && type_info_eq(&before.type_info, &after.type_info)
        && before.visibility == after.visibility
    {
        return out;
    }

    let mk = |kind: BreakKind, tier: BreakTier, detail: String| BreakingChange {
        symbol_uid: before.uid.clone(),
        symbol_name: before.name.clone(),
        kind,
        tier,
        detail,
    };

    // Visibility narrowed: Public -> {Internal, Protected, Private}.
    if before.visibility == Visibility::Public
        && matches!(
            after.visibility,
            Visibility::Internal | Visibility::Protected | Visibility::Private
        )
    {
        out.push(mk(
            BreakKind::VisibilityNarrowed,
            BreakTier::Breaking,
            format!(
                "visibility narrowed from {} to {}",
                before.visibility, after.visibility
            ),
        ));
    }

    match (&before.type_info, &after.type_info) {
        (Some(bt), Some(at)) => {
            let bp = &bt.parameter_types;
            let ap = &at.parameter_types;

            if ap.len() > bp.len() {
                out.push(mk(
                    BreakKind::ParamAdded,
                    BreakTier::Breaking,
                    format!(
                        "parameter count increased from {} to {}",
                        bp.len(),
                        ap.len()
                    ),
                ));
            } else if ap.len() < bp.len() {
                out.push(mk(
                    BreakKind::ParamRemoved,
                    BreakTier::Breaking,
                    format!(
                        "parameter count decreased from {} to {}",
                        bp.len(),
                        ap.len()
                    ),
                ));
            } else {
                // Same arity. Detect a pure reorder (same name set, new order).
                let bn: Vec<&str> = bp.iter().map(|(n, _)| n.as_str()).collect();
                let an: Vec<&str> = ap.iter().map(|(n, _)| n.as_str()).collect();
                let mut bn_sorted = bn.clone();
                let mut an_sorted = an.clone();
                bn_sorted.sort_unstable();
                an_sorted.sort_unstable();

                if bn != an && bn_sorted == an_sorted {
                    out.push(mk(
                        BreakKind::ParamReordered,
                        BreakTier::Breaking,
                        format!(
                            "parameters reordered from [{}] to [{}]",
                            bn.join(", "),
                            an.join(", ")
                        ),
                    ));
                } else {
                    // Positional type comparison (names identical, or changed but
                    // not a pure reorder — conservatively compare by position).
                    for (bpar, apar) in bp.iter().zip(ap.iter()) {
                        if bpar.1 != apar.1 {
                            out.push(mk(
                                BreakKind::ParamRetyped,
                                type_change_tier(&bpar.1, &apar.1),
                                format!(
                                    "parameter `{}` type changed from {} to {}",
                                    apar.0,
                                    ty_str(&bpar.1),
                                    ty_str(&apar.1)
                                ),
                            ));
                        }
                    }
                }
            }

            if bt.return_type != at.return_type {
                out.push(mk(
                    BreakKind::ReturnTypeChanged,
                    type_change_tier(&bt.return_type, &at.return_type),
                    format!(
                        "return type changed from {} to {}",
                        ty_str(&bt.return_type),
                        ty_str(&at.return_type)
                    ),
                ));
            }

            if bt.declared_type != at.declared_type {
                out.push(mk(
                    BreakKind::DeclaredTypeChanged,
                    type_change_tier(&bt.declared_type, &at.declared_type),
                    format!(
                        "declared type changed from {} to {}",
                        ty_str(&bt.declared_type),
                        ty_str(&at.declared_type)
                    ),
                ));
            }
        }
        // Either side lacks `type_info`: fall back to a coarse, conservative
        // signature comparison. Only a differing signature yields a single
        // `ReachOnly` hint — we can't attribute it to a specific parameter.
        _ => {
            if before.signature != after.signature {
                out.push(mk(
                    BreakKind::ParamRetyped,
                    BreakTier::ReachOnly,
                    format!(
                        "signature changed from `{}` to `{}` (type_info unavailable)",
                        before.signature, after.signature
                    ),
                ));
            }
        }
    }

    out
}

/// Diff two sets of symbols representing the public API before and after a
/// change, returning all breaking changes.
///
/// `after` symbols are indexed by [`stable_key`]. Each public-facing `before`
/// symbol is either matched (and delegated to [`diff_symbol`]), detected as a
/// rename, or reported as removed. Added symbols are non-breaking and skipped.
/// The result is sorted by `(symbol_uid, kind)` for deterministic output.
pub fn diff_public_api(before: &[Symbol], after: &[Symbol]) -> Vec<BreakingChange> {
    let mut after_by_key: HashMap<String, &Symbol> = HashMap::with_capacity(after.len());
    for s in after {
        after_by_key.insert(stable_key(s), s);
    }

    // An `after` symbol is a rename candidate only if nothing in `before` shares
    // its key (i.e. it wasn't matched positionally to some prior symbol).
    let before_keys: HashSet<String> = before.iter().map(stable_key).collect();
    let unmatched_after: Vec<&Symbol> = after
        .iter()
        .filter(|s| !before_keys.contains(&stable_key(s)))
        .collect();

    let mut out = Vec::new();

    for b in before {
        if !is_public_facing(b.visibility) {
            continue;
        }

        let key = stable_key(b);
        if let Some(a) = after_by_key.get(&key) {
            out.extend(diff_symbol(b, a));
            continue;
        }

        // No key match: try the rename heuristic before declaring removal.
        let renamed = unmatched_after.iter().find(|a| {
            a.file_path == b.file_path
                && a.name != b.name
                && a.signature == b.signature
                && type_info_eq(&a.type_info, &b.type_info)
        });

        if let Some(r) = renamed {
            out.push(BreakingChange {
                symbol_uid: b.uid.clone(),
                symbol_name: b.name.clone(),
                kind: BreakKind::Renamed,
                tier: BreakTier::Breaking,
                detail: format!("symbol renamed from `{}` to `{}`", b.name, r.name),
            });
        } else {
            out.push(BreakingChange {
                symbol_uid: b.uid.clone(),
                symbol_name: b.name.clone(),
                kind: BreakKind::Removed,
                tier: BreakTier::Breaking,
                detail: format!("public symbol `{}` removed", b.name),
            });
        }
    }

    out.sort_by(|x, y| {
        x.symbol_uid
            .cmp(&y.symbol_uid)
            .then_with(|| kind_rank(x.kind).cmp(&kind_rank(y.kind)))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{SymbolKind, TypeInfo, Visibility};

    /// Base symbol with all fields populated; tests clone and tweak.
    fn base() -> Symbol {
        Symbol {
            uid: "sym:1".to_string(),
            name: "foo".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 5,
            signature: "fn foo()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn ti(params: Vec<(&str, Option<&str>)>, ret: Option<&str>, decl: Option<&str>) -> TypeInfo {
        TypeInfo {
            declared_type: decl.map(String::from),
            parameter_types: params
                .into_iter()
                .map(|(n, t)| (n.to_string(), t.map(String::from)))
                .collect(),
            return_type: ret.map(String::from),
        }
    }

    #[test]
    fn removed_public_symbol() {
        let b = base();
        let changes = diff_public_api(&[b], &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::Removed);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn renamed_symbol_same_signature() {
        let mut b = base();
        b.signature = "fn foo(x: i32)".to_string();
        b.type_info = Some(ti(vec![("x", Some("i32"))], None, None));

        let mut a = b.clone();
        a.uid = "sym:2".to_string();
        a.name = "renamed_foo".to_string();
        // same file_path, signature, type_info — only the name differs.

        let changes = diff_public_api(&[b], &[a]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::Renamed);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn visibility_narrowed() {
        let b = base();
        let mut a = b.clone();
        a.visibility = Visibility::Private;

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::VisibilityNarrowed);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn param_added() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32"))], None, None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![("a", Some("i32")), ("b", Some("i32"))], None, None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamAdded);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn param_removed() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32")), ("b", Some("i32"))], None, None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![("a", Some("i32"))], None, None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamRemoved);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn param_reordered() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32")), ("b", Some("str"))], None, None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![("b", Some("str")), ("a", Some("i32"))], None, None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamReordered);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
    }

    #[test]
    fn param_retyped_both_resolved_is_likely_breaking() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32"))], None, None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![("a", Some("i64"))], None, None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamRetyped);
        assert_eq!(changes[0].tier, BreakTier::LikelyBreaking);
    }

    #[test]
    fn param_retyped_unresolved_side_is_reach_only() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32"))], None, None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![("a", None)], None, None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamRetyped);
        assert_eq!(changes[0].tier, BreakTier::ReachOnly);
    }

    #[test]
    fn return_type_changed() {
        let mut b = base();
        b.type_info = Some(ti(vec![], Some("i32"), None));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![], Some("i64"), None));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ReturnTypeChanged);
        assert_eq!(changes[0].tier, BreakTier::LikelyBreaking);
    }

    #[test]
    fn declared_type_changed() {
        let mut b = base();
        b.kind = SymbolKind::Constant;
        b.type_info = Some(ti(vec![], None, Some("i32")));
        let mut a = b.clone();
        a.type_info = Some(ti(vec![], None, Some("str")));

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::DeclaredTypeChanged);
        assert_eq!(changes[0].tier, BreakTier::LikelyBreaking);
    }

    #[test]
    fn body_only_change_is_not_a_break() {
        let mut b = base();
        b.type_info = Some(ti(vec![("a", Some("i32"))], Some("i32"), None));
        let mut a = b.clone();
        // Only the implementation changed: content_hash differs, API surface same.
        a.content_hash = "h2-different".to_string();

        let changes = diff_symbol(&b, &a);
        assert!(
            changes.is_empty(),
            "content_hash-only change must not be flagged: {changes:?}"
        );
    }

    #[test]
    fn added_symbol_is_not_a_break() {
        let b = base();
        let existing = b.clone(); // identical, matched -> no change
        let mut added = base();
        added.uid = "sym:new".to_string();
        added.name = "new_fn".to_string();
        added.file_path = "src/new.rs".to_string();

        let changes = diff_public_api(&[b], &[existing, added]);
        assert!(
            changes.is_empty(),
            "added symbol must not break: {changes:?}"
        );
    }

    #[test]
    fn visibility_widened_is_not_a_break() {
        let mut b = base();
        b.visibility = Visibility::Private;
        let mut a = b.clone();
        a.visibility = Visibility::Public;

        // API gate: non-public `before` is not part of the API surface.
        let changes = diff_symbol(&b, &a);
        assert!(changes.is_empty(), "widening must not break: {changes:?}");
    }

    #[test]
    fn private_symbol_change_is_gated_out() {
        let mut b = base();
        b.visibility = Visibility::Private;
        b.type_info = Some(ti(vec![("a", Some("i32"))], None, None));
        let mut a = b.clone();
        // A real signature change, but on a private symbol.
        a.signature = "fn foo(a: i32, b: i32)".to_string();
        a.type_info = Some(ti(vec![("a", Some("i32")), ("b", Some("i32"))], None, None));

        let changes = diff_symbol(&b, &a);
        assert!(
            changes.is_empty(),
            "private change must be gated: {changes:?}"
        );
    }

    #[test]
    fn coarse_signature_fallback_without_type_info_is_reach_only() {
        let mut b = base();
        b.signature = "fn foo(a: i32)".to_string();
        // No type_info on either side -> coarse signature comparison.
        let mut a = b.clone();
        a.signature = "fn foo(a: i64)".to_string();

        let changes = diff_symbol(&b, &a);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::ParamRetyped);
        assert_eq!(changes[0].tier, BreakTier::ReachOnly);
    }

    #[test]
    fn inferred_visibility_is_treated_as_public() {
        let mut b = base();
        b.visibility = Visibility::Inferred;
        let changes = diff_public_api(&[b], &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BreakKind::Removed);
    }
}

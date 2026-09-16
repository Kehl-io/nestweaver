//! Shared "did you mean" candidate builder for `impact`'s not-found response
//! (nw-481).
//!
//! `impact` resolves a bare name via EXACT match only (`resolve_uid` /
//! `tool_brain_impact`'s name branch both use
//! `store.lookup_symbols_by_name`), while `search` matches substrings — so a
//! name that `search` finds ten hits for can still be `impact`'s
//! `not_found`. This module builds the bounded candidate list surfaced as
//! `did_you_mean` on a not-found response, so the CLI (direct + daemon-JSON)
//! and MCP routes call ONE function instead of three copies of the same
//! substring lookup that could drift apart on limit, ordering, or scoping.

use nestweaver_schema::Symbol;
use nestweaver_store::{GraphStore, SeedResolutionConfig};

/// Bound on the number of suggested candidate names, per the nw-481 spec
/// ("≤5 entries") and verdict ("`search_symbols_page(limit 5)`"). A named
/// constant so every call site shares one number instead of a repeated
/// magic `5`.
pub const DID_YOU_MEAN_LIMIT: usize = 5;

/// How large a page to request from the store before `keep` and dedup run.
///
/// Requesting exactly [`DID_YOU_MEAN_LIMIT`] rows and filtering afterward
/// would starve recall: if any of the top 5 ranked matches are invisible to
/// the caller (wrong repo, restricted authz) or duplicate an already-seen
/// name, a real visible candidate ranked 6th or lower is silently dropped
/// even though it would have made the cut. Over-fetching a larger page and
/// filtering/deduping before truncating trades a slightly larger page (still
/// far under `SEARCH_PRESENTATION_LIMIT_MAX`) for correct recall, and this
/// function NEVER returns a name `keep` rejected — the wider fetch only
/// gives more visible candidates a chance to reach the final 5.
pub const DID_YOU_MEAN_OVER_FETCH: usize = DID_YOU_MEAN_LIMIT * 4;

/// Build a bounded, deterministic list of "did you mean" candidate symbol
/// names for an `impact` not-found response.
///
/// Reuses the store's existing substring symbol-name search
/// (`GraphStore::search_symbols_by_name_page`, the same ranked lookup
/// `search` and `nestweaver_engine::query::search_symbols_page` already use)
/// rather than a new full-table scan. Ranking (exact > prefix > contains,
/// then path/kind tie-breaks, then file path) comes from that search and is
/// already deterministic.
///
/// `query` MUST be a bare name, never a UID. Every caller already branches
/// on `name_or_uid.contains(':')` to route UID input to an exact lookup
/// (`resolve_uid`, `tool_brain_impact`'s UID branch); a UID substring-matched
/// against symbol NAMES is never a useful suggestion, so callers should skip
/// calling this entirely for UID input. (nw-481 counterweight: `impact
/// <uid-with-colon>` carries no `did_you_mean` key.) This function ALSO
/// checks `query.contains(':')` itself and returns an empty list — defense
/// in depth, so a future caller that forgets that branch still fails safe
/// instead of substring-searching symbol names for a UID and returning
/// nonsense. A query that is empty, or whitespace-only after trimming, is
/// rejected the same way: an empty substring pattern matches every symbol,
/// which would return arbitrary names rather than a genuine suggestion.
///
/// `keep` filters each candidate [`Symbol`]. Callers apply repo/`--repo`
/// scoping and authz visibility here (e.g. `nestweaver_engine::authz::repo_is_visible(&s.repo_uid,
/// visible)`), so the CLI and MCP routes cannot drift on WHAT counts as
/// visible while sharing WHERE the candidates come from. (nw-481
/// counterweight: `--repo other` excludes candidates from other repos.) The
/// filter runs over an over-fetched page (see [`DID_YOU_MEAN_OVER_FETCH`])
/// so a candidate `keep` rejects never crowds out a lower-ranked visible one.
/// Results are also deduplicated by name, preserving rank order, before
/// truncating to [`DID_YOU_MEAN_LIMIT`] — an overload set or the same name
/// visible in two repos must not spend the bound on repeats of one string.
///
/// Returns an empty `Vec`, never a placeholder, when nothing matches — so
/// callers can render `did_you_mean` as fully absent
/// (`skip_serializing_if`-style) rather than an always-present empty array.
/// (nw-481 counterweight: a name with zero substring hits has no
/// `did_you_mean` key.)
///
/// # Errors
/// Returns `Err` only on a store failure. This is a best-effort suggestion
/// lookup, not part of `impact`'s core contract: a caller MUST log the error
/// and fall back to the base not-found response with no `did_you_mean` key,
/// never fail (or change the exit code of) the whole `impact` command
/// because a suggestion lookup failed.
pub fn did_you_mean_candidates(
    store: &GraphStore,
    query: &str,
    keep: impl Fn(&Symbol) -> bool,
) -> Result<Vec<String>, anyhow::Error> {
    if query.contains(':') || query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let page = store
        .search_symbols_by_name_page(
            query,
            DID_YOU_MEAN_OVER_FETCH,
            &SeedResolutionConfig::default(),
        )
        .map_err(|e| anyhow::anyhow!("search_symbols_by_name_page: {e}"))?;

    let mut seen_names = std::collections::HashSet::new();
    let mut names = Vec::with_capacity(DID_YOU_MEAN_LIMIT);
    for candidate in page.symbols.iter().filter(|s| keep(s)) {
        if names.len() == DID_YOU_MEAN_LIMIT {
            break;
        }
        if seen_names.insert(candidate.name.clone()) {
            names.push(candidate.name.clone());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{SymbolKind, Visibility};

    fn symbol(uid: &str, name: &str, repo_uid: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: format!("src/{uid}.rs"),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: Some(format!("canonical:{uid}")),
        }
    }

    fn always_keep(_: &Symbol) -> bool {
        true
    }

    #[test]
    fn finds_substring_matches_impact_missed_on_exact_match() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("s1", "tool_project_context", "repo:a"))
            .unwrap();

        // `impact project_context` is not_found (no symbol is named exactly
        // "project_context"), but `search project_context` — and therefore
        // this builder — finds the substring hit.
        let names = did_you_mean_candidates(&store, "project_context", always_keep).unwrap();

        assert_eq!(names, vec!["tool_project_context".to_string()]);
    }

    /// Counterweight: a genuine miss (no substring hits at all) returns an
    /// empty, not fabricated, list.
    #[test]
    fn returns_empty_for_no_substring_hits() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("s1", "tool_project_context", "repo:a"))
            .unwrap();

        let names = did_you_mean_candidates(&store, "totally_bogus_name_xyz", always_keep).unwrap();

        assert!(names.is_empty());
    }

    /// Counterweight: `keep` (the caller's repo/`--repo`/visibility filter)
    /// excludes candidates from a repo the caller cannot see, mirroring
    /// nw-481's "`--repo other` excludes candidates from other repos".
    #[test]
    fn keep_predicate_excludes_symbols_outside_the_caller_scope() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("s1", "tool_project_context", "repo:a"))
            .unwrap();
        store
            .insert_symbol(&symbol("s2", "build_project_context", "repo:b"))
            .unwrap();

        let names =
            did_you_mean_candidates(&store, "project_context", |s| s.repo_uid == "repo:a").unwrap();

        assert_eq!(names, vec!["tool_project_context".to_string()]);
    }

    /// The list is bounded even when many symbols would otherwise match, and
    /// stable across repeated calls on the same fixture (deterministic
    /// ordering from `search_symbols_by_name_page`, not iteration-order luck).
    #[test]
    fn is_bounded_to_the_limit_and_deterministic() {
        let store = GraphStore::in_memory().unwrap();
        for i in 0..(DID_YOU_MEAN_LIMIT + 5) {
            store
                .insert_symbol(&symbol(
                    &format!("s{i}"),
                    &format!("candidate_match_{i}"),
                    "repo:a",
                ))
                .unwrap();
        }

        let first = did_you_mean_candidates(&store, "candidate_match", always_keep).unwrap();
        let second = did_you_mean_candidates(&store, "candidate_match", always_keep).unwrap();

        assert_eq!(first.len(), DID_YOU_MEAN_LIMIT);
        assert_eq!(first, second);
    }

    /// Regression for filter-after-limit starvation: if the store page were
    /// capped at exactly `DID_YOU_MEAN_LIMIT` before `keep` ran, the two
    /// visible candidates ranked 11th/12th would never be fetched at all.
    /// Over-fetching must surface them.
    #[test]
    fn over_fetches_so_a_visible_candidate_ranked_below_the_limit_is_not_starved() {
        let store = GraphStore::in_memory().unwrap();
        // Ranked ahead (by file-path tiebreak: "src/s00.rs" < ... < "src/s09.rs"),
        // but every one is in a repo `keep` rejects.
        for i in 0..10 {
            store
                .insert_symbol(&symbol(
                    &format!("s{i:02}"),
                    &format!("candidate_{i:02}"),
                    "repo:hidden",
                ))
                .unwrap();
        }
        // Ranked last, but visible.
        store
            .insert_symbol(&symbol("s10", "candidate_10", "repo:visible"))
            .unwrap();
        store
            .insert_symbol(&symbol("s11", "candidate_11", "repo:visible"))
            .unwrap();

        let names =
            did_you_mean_candidates(&store, "candidate", |s| s.repo_uid == "repo:visible").unwrap();

        assert_eq!(
            names,
            vec!["candidate_10".to_string(), "candidate_11".to_string()]
        );
    }

    /// Regression for duplicate names: an overload set, or the same name
    /// visible in two repos, must collapse to one suggestion rather than
    /// spending the bound on repeats of one string.
    #[test]
    fn dedupes_repeated_names_preserving_rank_order() {
        let store = GraphStore::in_memory().unwrap();
        let mut a = symbol("s1", "dup_name", "repo:a");
        a.file_path = "src/a.rs".to_string();
        let mut b = symbol("s2", "dup_name", "repo:b");
        b.file_path = "src/b.rs".to_string();
        store.insert_symbol(&a).unwrap();
        store.insert_symbol(&b).unwrap();

        let names = did_you_mean_candidates(&store, "dup_name", always_keep).unwrap();

        assert_eq!(names, vec!["dup_name".to_string()]);
    }

    /// Defense in depth: even if a future caller forgets the
    /// `name_or_uid.contains(':')` branch, a UID-shaped query must never
    /// substring-search symbol names and return nonsense.
    #[test]
    fn returns_empty_for_uid_shaped_query() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("s1", "sym:repo:a:hash:1", "repo:a"))
            .unwrap();

        let names = did_you_mean_candidates(&store, "sym:repo:a:hash:1", always_keep).unwrap();

        assert!(names.is_empty());
    }

    /// An empty (or whitespace-only) query is not a real substring pattern —
    /// it would match every symbol — so it must not fabricate suggestions.
    #[test]
    fn returns_empty_for_blank_query() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&symbol("s1", "tool_project_context", "repo:a"))
            .unwrap();

        for blank in ["", "   "] {
            let names = did_you_mean_candidates(&store, blank, always_keep).unwrap();
            assert!(
                names.is_empty(),
                "blank query {blank:?} must not match everything"
            );
        }
    }
}

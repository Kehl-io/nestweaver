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
/// `query` is a bare name or a `path::name` spec, never a UID. A
/// `path::name` miss (nw-113) is searched by its name half and each
/// suggestion is returned as `path::name`, a spec that resolves to that
/// symbol. Every caller already branches
/// on `name_or_uid.contains(':')` to route UID input to an exact lookup
/// (`resolve_uid`, `tool_brain_impact`'s UID branch); a UID substring-matched
/// against symbol NAMES is never a useful suggestion, so callers should skip
/// calling this entirely for UID input. (nw-481 counterweight: `impact
/// <uid-with-colon>` carries no `did_you_mean` key.) This function ALSO
/// returns an empty list for any other `:` form itself — defense
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
    // nw-113: a qualified miss is searched by its last segment and answered
    // with `path::name` specs (`path::Container::name` for a member); any
    // other `:` form (a UID) gets nothing.
    let qualified = if query.starts_with("sym:") {
        None
    } else {
        nestweaver_schema::QualifiedSpec::parse(query)
    };
    let search = qualified.as_ref().map_or(query, |spec| spec.name);
    if search.contains(':') || search.trim().is_empty() {
        return Ok(Vec::new());
    }

    // 12.1.1 (M4): `path::nosuch` is answered from that file's members, as
    // the CLI help promises.
    if let Some(spec) = qualified.as_ref() {
        let members = file_member_suggestions(store, spec, &keep)?;
        if !members.is_empty() {
            return Ok(members);
        }
    }

    let page = store
        .search_symbols_by_name_page(
            search,
            DID_YOU_MEAN_OVER_FETCH,
            &SeedResolutionConfig::default(),
        )
        .map_err(|e| anyhow::anyhow!("search_symbols_by_name_page: {e}"))?;
    // 12.1.1 (B1/M1): a `path::<module>` miss is answered only with owners
    // of a file of the same name. The first five owners in the graph, in
    // path order, are no suggestion for a file that has none.
    let same_file_name = |s: &Symbol| match qualified.as_ref() {
        Some(spec) if spec.name == nestweaver_schema::MODULE_OWNER_NAME => {
            let base = |path: &str| path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
            base(&s.file_path) == base(spec.qualifiers[0])
        }
        _ => true,
    };
    let mut visible: Vec<Symbol> = page
        .symbols
        .into_iter()
        .filter(|s| keep(s) && same_file_name(s))
        .collect();
    // 12.1.1 (M4): then names a small typo away, which no substring search
    // finds (`helpr`, `resolve_trigram_polcy`), closest first.
    if search != nestweaver_schema::MODULE_OWNER_NAME {
        let already: std::collections::HashSet<String> =
            visible.iter().map(|s| s.uid.clone()).collect();
        visible.extend(
            edit_distance_matches(store, search, &keep)?
                .into_iter()
                .filter(|s| !already.contains(&s.uid)),
        );
    }
    format_suggestions(store, qualified.is_some(), &visible)
}

/// The furthest edit distance a suggestion may be from a name of `len`
/// characters: none below 3, one for 3 or 4, two from 5.
fn max_edit_distance(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3 | 4 => 1,
        _ => 2,
    }
}

/// Levenshtein distance between `a` and `b` when it is at most `max`.
fn bounded_edit_distance(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = Vec::with_capacity(b.len() + 1);
        current.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        if current.iter().min().is_some_and(|&best| best > max) {
            return None;
        }
        previous = current;
    }
    previous.last().copied().filter(|&distance| distance <= max)
}

/// Symbols whose name is within [`max_edit_distance`] of `name`, case
/// insensitively, closest first. Module owners never match: they are named
/// only by `path::<module>`.
fn edit_distance_matches(
    store: &GraphStore,
    name: &str,
    keep: &impl Fn(&Symbol) -> bool,
) -> Result<Vec<Symbol>, anyhow::Error> {
    let needle: Vec<char> = name.to_lowercase().chars().collect();
    let max = max_edit_distance(needle.len());
    if max == 0 {
        return Ok(Vec::new());
    }
    let mut found = store
        .scan_symbol_names(|lower, symbol| {
            // Cheap length bound first: most of the table is skipped here.
            if symbol.is_module_owner()
                || lower.chars().count().abs_diff(needle.len()) > max
                || !keep(symbol)
            {
                return None;
            }
            let candidate: Vec<char> = lower.chars().collect();
            bounded_edit_distance(&needle, &candidate, max)
                .filter(|&distance| distance > 0)
                .map(|distance| (distance, symbol.clone()))
        })
        .map_err(|e| anyhow::anyhow!("scan_symbol_names: {e}"))?;
    found.sort_by(|(da, a), (db, b)| {
        da.cmp(db)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });
    found.truncate(DID_YOU_MEAN_OVER_FETCH);
    Ok(found.into_iter().map(|(_, symbol)| symbol).collect())
}

/// 12.1.1 (M4): when `spec`'s first qualifier names a file, that file's
/// members as `path::name` suggestions (best file match only, like the
/// lookup): those whose name is close to the one asked for (a substring
/// either way, or a small typo), else every member in source order. An inner
/// class qualifier narrows to that class's members when it has any. Empty
/// when no file matches.
fn file_member_suggestions(
    store: &GraphStore,
    spec: &nestweaver_schema::QualifiedSpec<'_>,
    keep: &impl Fn(&Symbol) -> bool,
) -> Result<Vec<String>, anyhow::Error> {
    let qualifier = spec.qualifiers[0];
    let mut members = store
        .scan_symbol_names(|_, symbol| {
            if symbol.is_module_owner() || !keep(symbol) {
                return None;
            }
            nestweaver_schema::file_qualifier_rank(qualifier, &symbol.file_path)
                .map(|rank| (rank, symbol.clone()))
        })
        .map_err(|e| anyhow::anyhow!("scan_symbol_names: {e}"))?;
    let Some(best) = members.iter().map(|(rank, _)| *rank).min() else {
        return Ok(Vec::new());
    };
    members.retain(|(rank, _)| *rank == best);
    let mut members: Vec<Symbol> = members.into_iter().map(|(_, symbol)| symbol).collect();
    members.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
    });
    let uids: Vec<&str> = members.iter().map(|s| s.uid.as_str()).collect();
    let containers = store
        .container_chains(&uids, 1)
        .map_err(|e| anyhow::anyhow!("container_chains: {e}"))?;
    if let Some(class) = spec.qualifiers.get(1..).and_then(<[&str]>::last) {
        let in_class: Vec<Symbol> = members
            .iter()
            .filter(|s| {
                containers
                    .get(&s.uid)
                    .and_then(|chain| chain.first())
                    .is_some_and(|container| container == class)
            })
            .cloned()
            .collect();
        if !in_class.is_empty() {
            members = in_class;
        }
    }
    let needle = spec.name.to_lowercase();
    let needle_chars: Vec<char> = needle.chars().collect();
    let max = max_edit_distance(needle_chars.len());
    let mut close: Vec<(usize, &Symbol)> = members
        .iter()
        .filter_map(|s| {
            let lower = s.name.to_lowercase();
            if lower.contains(&needle) || needle.contains(&lower) {
                return Some((0, s));
            }
            let chars: Vec<char> = lower.chars().collect();
            bounded_edit_distance(&needle_chars, &chars, max).map(|distance| (distance, s))
        })
        .collect();
    close.sort_by_key(|(distance, _)| *distance);
    let chosen: Vec<Symbol> = if close.is_empty() {
        members
    } else {
        close.into_iter().map(|(_, s)| s.clone()).collect()
    };
    format_suggestions(store, true, &chosen)
}

/// Render `symbols` as up to [`DID_YOU_MEAN_LIMIT`] distinct suggestions:
/// `path::name` (`path::Container::name` for a member) when the miss was
/// qualified, the display name otherwise.
fn format_suggestions(
    store: &GraphStore,
    qualified: bool,
    symbols: &[Symbol],
) -> Result<Vec<String>, anyhow::Error> {
    // One container level is enough for the suggestion to resolve: the
    // lookup matches a container chain from its innermost end.
    let containers = if qualified {
        let uids: Vec<&str> = symbols.iter().map(|s| s.uid.as_str()).collect();
        store
            .container_chains(&uids, 1)
            .map_err(|e| anyhow::anyhow!("container_chains: {e}"))?
    } else {
        Default::default()
    };

    let mut seen_names = std::collections::HashSet::new();
    let mut names = Vec::with_capacity(DID_YOU_MEAN_LIMIT);
    for candidate in symbols {
        if names.len() == DID_YOU_MEAN_LIMIT {
            break;
        }
        let suggestion = if qualified {
            match containers
                .get(&candidate.uid)
                .and_then(|chain| chain.first())
            {
                Some(container) => {
                    format!("{}::{container}::{}", candidate.file_path, candidate.name)
                }
                None => format!("{}::{}", candidate.file_path, candidate.name),
            }
        } else {
            candidate.display_name().into_owned()
        };
        if seen_names.insert(suggestion.clone()) {
            names.push(suggestion);
        }
    }
    Ok(names)
}

/// The one text rendering of `did_you_mean` suggestions every CLI miss
/// prints: `Did you mean: a, b?`. `None` when there are none, so no line is
/// printed.
pub fn did_you_mean_line(candidates: &[String]) -> Option<String> {
    (!candidates.is_empty()).then(|| format!("Did you mean: {}?", candidates.join(", ")))
}

/// nw-113: suggestions for every qualified seed in `seeds` (`path::name`,
/// `Class::method`), merged in seed order, deduplicated and bounded by
/// [`DID_YOU_MEAN_LIMIT`]. `context` and `brain context` call this when no
/// seed resolved; a bare-name seed gets none, since those commands fall back
/// to a substring search that already had its chance.
///
/// # Errors
/// Only on a store failure; callers treat it as best-effort like
/// [`did_you_mean_candidates`].
pub fn qualified_seed_suggestions(
    store: &GraphStore,
    seeds: &[String],
    keep: impl Fn(&Symbol) -> bool,
) -> Result<Vec<String>, anyhow::Error> {
    let mut names: Vec<String> = Vec::new();
    for seed in seeds {
        let seed = seed.trim();
        if seed.starts_with("sym:") || nestweaver_schema::QualifiedSpec::parse(seed).is_none() {
            continue;
        }
        for name in did_you_mean_candidates(store, seed, &keep)? {
            if names.len() == DID_YOU_MEAN_LIMIT {
                return Ok(names);
            }
            if !names.contains(&name) {
                names.push(name);
            }
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

    /// nw-113: a qualified miss is searched by its name half, and each
    /// suggestion is a `path::name` spec that resolves to that symbol.
    #[test]
    fn qualified_miss_suggests_path_qualified_names() {
        let store = GraphStore::in_memory().unwrap();
        let mut listen = symbol("s1", "listen", "repo:a");
        listen.file_path = "src/server.js".to_string();
        store.insert_symbol(&listen).unwrap();

        let names = did_you_mean_candidates(&store, "nope/missing.js::liste", always_keep).unwrap();
        assert_eq!(names, vec!["src/server.js::listen".to_string()]);
        assert!(
            did_you_mean_candidates(&store, "sym:repo:a:x", always_keep)
                .unwrap()
                .is_empty(),
            "a UID gets no suggestion"
        );
    }

    /// nw-113 final round: a `Class::method` or `path::Class::method` miss
    /// suggests specs that keep the container, so each one resolves to a
    /// single method; a bare-name suggestion still drops nothing.
    #[test]
    fn qualified_miss_suggestions_keep_the_container_chain() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};
        let store = GraphStore::in_memory().unwrap();
        for (uid, name, path, parent) in [
            ("a-svc", "Svc", "a/Svc.ts", None),
            ("a-run", "run", "a/Svc.ts", Some("a-svc")),
            ("b-svc", "Svc", "b.ts", None),
            ("b-run", "run", "b.ts", Some("b-svc")),
        ] {
            let mut s = symbol(uid, name, "repo:a");
            s.file_path = path.to_string();
            store.insert_symbol(&s).unwrap();
            if let Some(parent) = parent {
                store
                    .insert_edge(&ResolvedEdge {
                        source_uid: uid.to_string(),
                        target_uid: parent.to_string(),
                        edge_type: EdgeType::MemberOf,
                        confidence: 1.0,
                        link_type: None,
                        evidence: Vec::new(),
                    })
                    .unwrap();
            }
        }
        let mut names = did_you_mean_candidates(&store, "WrongClass::run", always_keep).unwrap();
        names.sort();
        assert_eq!(names, ["a/Svc.ts::Svc::run", "b.ts::Svc::run"]);
        let mut names =
            did_you_mean_candidates(&store, "src/a/svc.ts::Svc::run", always_keep).unwrap();
        names.sort();
        assert_eq!(names, ["a/Svc.ts::Svc::run", "b.ts::Svc::run"]);
    }

    /// 12.1.1 (M4): a typo the substring search cannot see (a dropped or
    /// swapped letter) is suggested by edit distance (at most 2, at most 1
    /// for a 3- or 4-letter name), after any substring match.
    #[test]
    fn edit_distance_typos_are_suggested() {
        let store = GraphStore::in_memory().unwrap();
        for (uid, name) in [
            ("s1", "resolve_trigram_policy"),
            ("s2", "helper"),
            ("s3", "run"),
            ("s4", "unrelated_thing"),
        ] {
            store.insert_symbol(&symbol(uid, name, "repo:a")).unwrap();
        }
        let ask = |query: &str| did_you_mean_candidates(&store, query, always_keep).unwrap();
        assert_eq!(ask("resolve_trigram_polcy"), ["resolve_trigram_policy"]);
        assert_eq!(ask("helpr"), ["helper"]);
        assert_eq!(ask("hepler"), ["helper"], "a swap is two edits");
        assert_eq!(ask("rum"), ["run"]);
        assert!(ask("xyz").is_empty(), "three edits from `run` is no match");
        assert_eq!(
            ask("nope.js::helpr"),
            ["src/s2.rs::helper"],
            "a qualified typo suggests path::name"
        );
        // A substring match ranks before an edit-distance one. (A fresh
        // store: the name table is cached per graph generation.)
        let store = GraphStore::in_memory().unwrap();
        for (uid, name) in [("s2", "helper"), ("s5", "helpr_cache")] {
            store.insert_symbol(&symbol(uid, name, "repo:a")).unwrap();
        }
        assert_eq!(
            did_you_mean_candidates(&store, "helpr", always_keep).unwrap(),
            ["helpr_cache", "helper"]
        );
    }

    /// 12.1.1 (M4): `path::nosuch` is answered from that file's members, as
    /// the CLI help promises, closest name first; with no close name, the
    /// file's members in source order. Never another file's symbols.
    #[test]
    fn a_file_qualified_miss_suggests_that_files_members() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};
        let store = GraphStore::in_memory().unwrap();
        for (uid, name, path, line, parent) in [
            ("u-repo", "UserRepository", "engine/user.py", 1, None),
            ("u-save", "save", "engine/user.py", 2, Some("u-repo")),
            ("u-load", "load", "engine/user.py", 5, Some("u-repo")),
            ("u-help", "user_helper", "engine/user.py", 9, None),
            ("o-save", "save", "engine/order.py", 1, None),
        ] {
            let mut s = symbol(uid, name, "repo:a");
            s.file_path = path.to_string();
            s.start_line = line;
            store.insert_symbol(&s).unwrap();
            if let Some(parent) = parent {
                store
                    .insert_edge(&ResolvedEdge {
                        source_uid: uid.to_string(),
                        target_uid: parent.to_string(),
                        edge_type: EdgeType::MemberOf,
                        confidence: 1.0,
                        link_type: None,
                        evidence: vec![],
                    })
                    .unwrap();
            }
        }
        let ask = |query: &str| did_you_mean_candidates(&store, query, always_keep).unwrap();
        assert_eq!(
            ask("engine/user.py::sav"),
            ["engine/user.py::UserRepository::save"],
            "the close member only, never order.py's save"
        );
        assert_eq!(
            ask("user.py::UserRepository::lod"),
            ["engine/user.py::UserRepository::load"]
        );
        assert_eq!(
            ask("user.py::nosuch"),
            [
                "engine/user.py::UserRepository",
                "engine/user.py::UserRepository::save",
                "engine/user.py::UserRepository::load",
                "engine/user.py::user_helper",
            ],
            "no close name: the file's members in source order"
        );
        assert!(
            ask("user.py::<module>")
                .iter()
                .all(|s| s.starts_with("engine/user.py::") && !s.ends_with("::<module>")),
            "an owner-less file suggests its members"
        );
    }

    /// One text rendering of the suggestions, for every CLI miss.
    #[test]
    fn did_you_mean_line_renders_or_is_absent() {
        assert_eq!(did_you_mean_line(&[]), None);
        assert_eq!(
            did_you_mean_line(&["a.ts::f".to_string(), "b.ts::f".to_string()]).as_deref(),
            Some("Did you mean: a.ts::f, b.ts::f?")
        );
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

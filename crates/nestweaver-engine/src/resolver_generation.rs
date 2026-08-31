//! Per-repo record of which resolver built a repo's edges.
//!
//! nw-124. Some resolver fixes change the SHAPE of the graph rather than the
//! query that reads it, so upgrading the binary does not correct data already
//! on disk. nw-103 is the motivating case: import edges used to be fanned out
//! to every symbol in the imported file, which put never-exported string
//! constants at the top of `hubs` with 800+ out-edges. The fix landed in
//! `resolve_references`, but that runs at INDEX time — so a user who upgrades
//! keeps the corrupted hub, bridge and PageRank rankings until each repo is
//! re-indexed, with nothing telling them the numbers are stale.
//!
//! Measured on the production graph: with the FIXED binary, `hubs` still
//! returned the exact ranking from the bug report. Re-indexing one repo removed
//! every one of its artefacts from the top 10.
//!
//! This sidecar makes that staleness visible instead of silent. It is
//! deliberately a sidecar and not a node property: adding a column to `Repo`
//! would need a graph-schema migration, and a repo indexed before this file
//! existed has no entry — which is exactly the "predates the fix" answer we
//! want, at zero migration cost.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Current resolver generation.
///
/// Bump this whenever a resolver change alters persisted edge shape such that
/// previously-indexed repos would yield different (wrong) analysis results.
/// Bumping it makes every not-yet-re-indexed repo report as stale.
///
/// 1 — nw-103: import edges attributed to the importing file instead of being
///     fanned out to every symbol in the imported file.
/// 2 — nw-308/nw-327: the nw-150 receiver gate reached only the same-package
///     fallback, so the direct-import and re-export tiers bound every bare
///     method name (`collect`, `contains`, `len`) to any same-named symbol in
///     an imported file. nw-323/nw-324: TS/JS `.js`-to-`.ts` specifiers,
///     `"./src/*"` tsconfig targets, `export … from` re-exports and
///     `new X()` all produced no edge at all. Both change edge SHAPE.
/// 3 — nw-349/nw-330/nw-340: functions in cpp, dart, cobol, svelte, vue and
///     astro recorded `end_line == start_line`, so `find_enclosing_symbol`
///     could not place a call inside the function containing it and the
///     degenerate-span fallback attributed it to the nearest preceding one-line
///     symbol of any kind — including `Constant`/`Variable`/`Property` symbols
///     that cannot be call sites. Both the spans and the fallback's kind
///     restriction change which SOURCE an edge is written from, and Rust `impl`
///     blocks changing from `Class` to `Extension` changes the UIDs those edges
///     point at. None of it is observable on a repo already indexed: the edges
///     are on disk with the old sources.
/// 4 — nw-352/nw-356: `.h` was dispatched to the C grammar, so every C++ header
///     was read by `queries/c.scm`. Measured on 874 real headers, moving it to
///     C++ takes them from 7,166 symbols to 17,872 and from 0 `class`
///     definitions to 1,373. Symbol UIDs are `(repo, path, name, start_line)`,
///     so both the symbol set and every edge endpoint in every header change,
///     and `#include` moves from `@reference.import` to `@reference.includes`.
///     nw-351: `find_parent_name` learned the C-family container node kinds, so
///     C and C++ members now mint MEMBER_OF edges that did not exist at all
///     before — a new edge family, invisible on a repo already indexed.
///     nw-349 (cross-lane): C++ `#include` now resolves instead of being
///     discarded, which is a new IMPORTS edge family for every C++ repo.
///     nw-364: julia call sites are no longer minted as definitions (UIDs
///     removed) while two previously-unreachable julia definitions appear (UIDs
///     added); every reference's persisted `context` changes; and svelte/vue/
///     astro named exports change `SymbolKind`, which the degenerate-span
///     fallback gates on. All of it is on-disk shape.
///     nw-349 cause 3: `queries/rust.scm` had no attribute capture of any
///     kind, so `#[serde(default = "f")]` — 97 sites and 31 distinct named
///     functions in this repo alone — produced NO reference and the named
///     function had in-degree 0. Adding edges that did not exist changes edge
///     shape, and nothing on an already-indexed repo can acquire them: the
///     edge set is on disk without them. Without this bump the fix is
///     invisible to every existing graph, which is the trap this module exists
///     for and which this codebase has now sprung twice (nw-103, and again in
///     round 3).
pub const RESOLVER_GENERATION: u32 = 4;

/// An unrecorded repo reads as generation 0, so the current generation must
/// stay above it — otherwise the pre-fix data this module exists to flag would
/// report as current.
const _: () = assert!(RESOLVER_GENERATION > 0);

/// Sidecar filename suffix, alongside the other `<db>.*` sidecars.
pub const RESOLVER_GENERATION_SIDECAR: &str = ".resolver_generation.json";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResolverGenerations {
    /// repo uid -> generation that produced its edges.
    #[serde(default)]
    pub repos: BTreeMap<String, u32>,
}

impl ResolverGenerations {
    /// Generation recorded for `repo_uid`. A repo with no entry was indexed
    /// before this record existed, which is generation 0 by definition.
    pub fn generation_for(&self, repo_uid: &str) -> u32 {
        self.repos.get(repo_uid).copied().unwrap_or(0)
    }

    /// Repo uids whose edges predate [`RESOLVER_GENERATION`], SORTED.
    ///
    /// nw-358. This is the sole computation behind every route's
    /// `stale_repos` — the CLI's two staleness constructors, `hub_nodes` /
    /// `bridge_nodes` over MCP, and `staleness_note` — and it used to preserve
    /// its caller's order. The callers enumerate different containers: a
    /// `BTreeMap`'s keys on one side (lexicographic INCIDENTALLY, with nothing
    /// stating the intent) and `MATCH (r:Repo) RETURN` with no `ORDER BY` on
    /// the other. So the same database answered in two byte-shapes depending
    /// on which one asked.
    ///
    /// Sorting HERE rather than in a printer is the point: a sort in
    /// `print_ranking_json` would fix the CLI's two legs and leave MCP
    /// unsorted, converting a three-way divergence into a two-way one. Here it
    /// also makes the answer stable across databases, not merely across
    /// routes on one.
    pub fn stale_repos<'a, I: IntoIterator<Item = &'a str>>(&self, known: I) -> Vec<String> {
        let mut stale: Vec<String> = known
            .into_iter()
            .filter(|uid| self.generation_for(uid) < RESOLVER_GENERATION)
            .map(|uid| uid.to_string())
            .collect();
        stale.sort_unstable();
        stale
    }
}

/// Load the sidecar, or an empty record when absent/unreadable.
///
/// Absent is not an error: it means every repo predates the record, which is
/// the correct reading for any database indexed before this shipped.
pub fn load(db_path: &Path) -> ResolverGenerations {
    let path = crate::sidecar_path(db_path, RESOLVER_GENERATION_SIDECAR);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record that `repo_uid` was just indexed by the current resolver.
///
/// Merges into the existing file so re-indexing one repo never claims the
/// others were refreshed too — the whole point is per-repo truth.
pub fn record(db_path: &Path, repo_uid: &str) -> Result<(), anyhow::Error> {
    let path = crate::sidecar_path(db_path, RESOLVER_GENERATION_SIDECAR);
    let mut current = load(db_path);
    current
        .repos
        .insert(repo_uid.to_string(), RESOLVER_GENERATION);
    std::fs::write(&path, serde_json::to_string_pretty(&current)?)?;
    Ok(())
}

/// Operator-facing caveat for a graph whose edges predate the current
/// resolver, or `None` when every repo is current.
///
/// Ranking commands (`hubs`, `bridges`, `repo-map`, `ranking rank` — anything
/// built on PageRank) are the surfaces nw-103 corrupted, and `stale-check` is
/// the command a user runs to ask "do I need to re-index?", so they are where
/// the disclosure belongs.
pub fn staleness_note(db_path: &Path, repo_uids: &[String]) -> Option<String> {
    let gens = load(db_path);
    let stale = gens.stale_repos(repo_uids.iter().map(|s| s.as_str()));
    staleness_note_for(&stale, Some(repo_uids.len()))
}

/// The same caveat, rendered from an ALREADY-COMPUTED stale set.
///
/// nw-365. The daemon path holds no store handle and cannot enumerate repos,
/// so it cannot supply the `of M` denominator — but it is NOT reduced to
/// guessing, because `attach_ranking_staleness` already ships the exact
/// `stale_repos` on every `hub_nodes` / `bridge_nodes` reply. Splitting the
/// renderer from the computation lets that route print the same sentence from
/// the answer it was sent, instead of a fourth hand-written one that would
/// drift from this one the first time either is edited.
///
/// `total` is `None` where the population is genuinely unknown. It is rendered
/// as a floor ("N repo(s) are known to have been") rather than by inventing a
/// denominator, because on that route `stale_repos` can UNDER-count: the
/// sidecar fallback for a pre-`attach_ranking_staleness` daemon sees only the
/// repos the sidecar records, and a repo present in the graph but absent from
/// the sidecar is stale and invisible to it. Claiming "N of N" there would be
/// a number this code cannot support.
///
/// `--force` in the remedy is LOAD-BEARING, not a flourish. This note used to
/// print `nestweaver index --repo <path>`, and that command is a no-op on the
/// exact state the note describes: a generation-stale repo is at HEAD with
/// every file unchanged, so incremental detection reports `0 added, 0
/// modified, 0 deleted`, writes nothing, and leaves the sidecar recording the
/// OLD generation. Measured on a generation-downgraded scratch DB: the sidecar
/// read `3` before and `3` after; only `--force` took it to `4`. A disclosure
/// whose remedy cannot clear the condition it discloses is worse than silence,
/// because the user runs it, sees success, and re-reads the same warning.
pub fn staleness_note_for(stale: &[String], total: Option<usize>) -> Option<String> {
    if stale.is_empty() {
        return None;
    }
    let scope = match total {
        Some(total) => format!("{} of {total} repo(s) were", stale.len()),
        None => format!("{} repo(s) are known to have been", stale.len()),
    };
    Some(format!(
        "{scope} indexed by an older resolver, so their edges are the ones that \
         resolver wrote: rankings over them are wrong, and edge families added since \
         (C/C++ MEMBER_OF, C++ IMPORTS) are absent entirely. Upgrading the binary does \
         not repair data already on disk. Re-index each one with \
         `nestweaver index --repo <path> --force`."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_entry_reads_as_generation_zero() {
        let g = ResolverGenerations::default();
        assert_eq!(g.generation_for("repo:whatever"), 0);
    }

    #[test]
    fn only_repos_below_the_current_generation_are_stale() {
        let mut g = ResolverGenerations::default();
        g.repos.insert("fresh".into(), RESOLVER_GENERATION);
        g.repos.insert("ancient".into(), 0);
        let stale = g.stale_repos(vec!["fresh", "ancient", "unrecorded"]);
        assert_eq!(stale, vec!["ancient".to_string(), "unrecorded".to_string()]);
    }

    #[test]
    fn recording_one_repo_does_not_mark_the_others_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        record(&db, "repo:a").unwrap();
        record(&db, "repo:b").unwrap();

        let g = load(&db);
        assert_eq!(g.generation_for("repo:a"), RESOLVER_GENERATION);
        assert_eq!(g.generation_for("repo:b"), RESOLVER_GENERATION);
        assert_eq!(g.generation_for("repo:never-indexed"), 0);
    }

    #[test]
    fn note_is_absent_when_everything_is_current_and_counts_when_not() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        record(&db, "repo:a").unwrap();

        assert!(staleness_note(&db, &["repo:a".to_string()]).is_none());

        let note = staleness_note(&db, &["repo:a".to_string(), "repo:b".to_string()])
            .expect("a stale repo must produce a caveat");
        assert!(note.contains("1 of 2"), "{note}");
        assert!(note.contains("indexed by an older resolver"), "{note}");
    }

    /// The remedy must carry `--force`.
    ///
    /// Without it the command is a no-op on a generation-stale repo: it is at
    /// HEAD with nothing modified, so incremental detection skips the write and
    /// the sidecar keeps recording the old generation. The un-forced form
    /// shipped for two releases; `tests/parity_test.rs` executes the remedy
    /// end-to-end so this assertion cannot pass on a string alone.
    #[test]
    fn the_remedy_forces_a_full_reindex_because_incremental_cannot_clear_this() {
        let note = staleness_note_for(&["repo:a".to_string()], Some(1))
            .expect("a stale repo must produce a caveat");
        assert!(
            note.contains("nestweaver index --repo <path> --force"),
            "plain `index` reports `0 modified` on a repo already at HEAD and \
             leaves the old edges — and the old generation — in place: {note}"
        );
    }
}

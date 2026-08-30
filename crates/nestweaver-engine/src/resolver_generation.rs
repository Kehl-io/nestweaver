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

    /// Repo uids whose edges predate [`RESOLVER_GENERATION`].
    pub fn stale_repos<'a, I: IntoIterator<Item = &'a str>>(&self, known: I) -> Vec<String> {
        known
            .into_iter()
            .filter(|uid| self.generation_for(uid) < RESOLVER_GENERATION)
            .map(|uid| uid.to_string())
            .collect()
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

/// Operator-facing caveat for ranking output, or `None` when every repo is
/// current.
///
/// Ranking commands (`hubs`, `bridges`, and anything built on PageRank) are
/// the surfaces nw-103 corrupted, so they are where the disclosure belongs.
pub fn staleness_note(db_path: &Path, repo_uids: &[String]) -> Option<String> {
    let gens = load(db_path);
    let stale = gens.stale_repos(repo_uids.iter().map(|s| s.as_str()));
    if stale.is_empty() {
        return None;
    }
    Some(format!(
        "{} of {} repo(s) were indexed by an older resolver, so their edges predate \
         the nw-103 import-fan-out fix — hub, bridge and PageRank rankings for those \
         repos are NOT corrected by upgrading alone. Re-index them \
         (`nestweaver index --repo <path>`) to get accurate rankings.",
        stale.len(),
        repo_uids.len()
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
        assert!(note.contains("nw-103"), "{note}");
    }
}

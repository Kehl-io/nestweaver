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

use anyhow::Context;
use nestweaver_schema::Repo;
use nestweaver_store::GraphStore;
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

    /// Repo uids whose edges were not produced by exactly
    /// [`RESOLVER_GENERATION`], SORTED.
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
            .filter(|uid| self.generation_for(uid) != RESOLVER_GENERATION)
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

/// Stable descriptor carried by every edge-dependent analysis that cannot
/// trust the resolver generation recorded for its graph.
pub const INCOMPATIBLE_RESOLVER_DESCRIPTOR: &str = "resolver-generation-incompatible";

/// Return every repository whose persisted edges are incompatible with the
/// running resolver.
///
/// In-memory stores have no persisted sidecar and are used heavily by pure
/// analysis tests, so there is no disk generation to prove or reject. Every
/// disk-backed store is checked, including absent and unreadable sidecars:
/// [`load`] maps both to generation zero, which is deliberately incompatible.
pub fn incompatible_repos_for_store(store: &GraphStore) -> anyhow::Result<Vec<String>> {
    let Some(db_path) = store.db_path() else {
        return Ok(Vec::new());
    };
    let repos = store
        .list_repos(None)
        .context("list repositories for resolver-generation compatibility")?;
    Ok(load(db_path).stale_repos(repos.iter().map(|repo| repo.uid.as_str())))
}

/// One shared diagnostic for affected-tests, detect-changes, and blast-radius.
pub fn incompatibility_message(repos: &[String]) -> String {
    format!(
        "edge-dependent analysis cannot trust repositories with a resolver generation that \
         differs from the running generation {}: {}. Re-index each repository with \
         `nestweaver index --repo <path> --force`",
        RESOLVER_GENERATION,
        repos.join(", ")
    )
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
/// as a floor ("N repo(s) are known to be incompatible") rather than by inventing a
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
        None => format!("{} repo(s) are known to be", stale.len()),
    };
    Some(format!(
        "{scope} incompatible with this resolver. This includes repositories indexed by an \
         older resolver, repositories with missing or unreadable generation metadata, and \
         repositories claiming a future generation this binary cannot understand. Their edges \
         cannot be trusted: rankings may be wrong, and edge families may be absent entirely. \
         Upgrading the binary does not repair data already on disk. Re-index each one with \
         `nestweaver index --repo <path> --force`."
    ))
}

/// Why `dead-code` REFUSES on a generation-stale graph instead of warning.
///
/// nw-372. Every other resolver-generation surface discloses and then prints
/// anyway, and that is right for them: their output is a RANKING, and a reader
/// told the order is suspect can discount the order. `dead-code`'s output is a
/// list of symbols to DELETE.
///
/// It is computed by a reachability BFS that walks FORWARD from entry points,
/// and a symbol is reported when the walk never arrived. So a MISSING edge can
/// only ever fail to reach a live symbol — the error is ONE-DIRECTIONAL and
/// always points at "delete this". On a pre-generation-4 graph the missing
/// edges are not a rounding error: C and C++ `MEMBER_OF` edges and C++
/// `IMPORTS` edges are absent ENTIRELY, so every C++ member reachable only
/// through its container falls out of the walk.
///
/// The costs are asymmetric and that settles it. Refusing costs one error
/// message and a re-index the user needs anyway. Printing costs a user
/// deleting live code, which the tool's output cannot undo — and this tool
/// already measures 0/15 top-15 precision on Rust with a CURRENT graph. A
/// warning printed above a deletion list is a pattern that has already failed
/// in this repository: the docs audit found a shipped skill telling agents
/// that unreachable code "may be safe to remove instead of fix".
const WHY_DEAD_CODE_REFUSES: &str = "dead-code will not produce a list on this graph. Its output \
     is a list of symbols to DELETE, computed by walking forward from entry points, so a MISSING \
     edge cannot make the list safer — it can only fail to reach a live symbol and report it as \
     dead. The error is one-directional and the deletion it invites is not recoverable.";

/// Why `affected-tests` REFUSES rather than degrading.
///
/// Lived as two copies -- `src/main.rs` and `nestweaver-mcp/src/tools.rs` --
/// each carrying a doc comment claiming byte-identity with the other. One
/// definition removes the claim and the drift it invited. Public because the
/// CLI also uses it as the fallback when a daemon-supplied refusal payload
/// carries no `note`.
pub const WHY_AFFECTED_TESTS_REFUSES: &str = "affected-tests will not produce a selection on this graph. Its output is the set of tests a \
     change can reach through the call/import graph, so a MISSING edge cannot make the selection \
     safer — it can only drop a test that should have run, while `status` still reads complete. \
     The error is one-directional and it is silent: nothing downstream can tell a test that was \
     not selected from a test that does not exist.";

/// One generation-stale repo, named the way the refusal names it.
///
/// `command` is a string the user can PASTE. `staleness_note_for` prints a
/// TEMPLATE — `nestweaver index --repo <path> --force`, with a literal
/// `<path>` — because that renderer is reached from routes holding no repo
/// rows. This one is built where the [`Repo`] rows are in hand, so it
/// substitutes the real path, and the parity test EXECUTES what it prints.
#[derive(Debug, Clone, Serialize)]
pub struct StaleRepoRemedy {
    /// Repo UID — the same population `stale-check`'s `resolver_stale_repos`
    /// carries, so a caller can join the two without decoding either.
    pub uid: String,
    /// The working tree to pass to `--repo`, when this machine has one.
    pub path: Option<String>,
    /// The exact command that clears THIS repo's staleness, or `None` when
    /// there is no local working tree to name. Never a command that cannot
    /// run: an unexecutable remedy is the defect nw-370 was fixing.
    pub command: Option<String>,
}

impl StaleRepoRemedy {
    fn new(uid: String, path: Option<String>) -> Self {
        let command = path
            .as_deref()
            .map(|path| format!("nestweaver index --repo {path} --force"));
        Self { uid, path, command }
    }
}

/// The verdict `dead-code` refuses on, shared by every route.
///
/// One value renders the stderr paragraph and the machine-readable payload, so
/// the CLI's direct route, the MCP tool the CLI's daemon route calls through,
/// and the MCP tool an agent calls directly cannot say three different things
/// about one database.
#[derive(Debug, Clone)]
pub enum DeadCodeRefusal {
    /// These repos' edges predate [`RESOLVER_GENERATION`].
    ///
    /// One variant, because there turned out to be exactly one state. The
    /// obvious second — "the verdict could not be computed, refuse anyway" —
    /// was written and then deleted once the callers stopped depending on the
    /// `CURRENT_DB_PATH` thread-local: `GraphStore::db_path` cannot be unset
    /// by a worker thread, a store that cannot enumerate repos propagates its
    /// own error, and an in-memory store has no disk for a stale sidecar to
    /// live on. Refusing on an unanswerable question is the right default; not
    /// having an unanswerable question is better.
    OutdatedResolver {
        repos: Vec<StaleRepoRemedy>,
        /// How many repos exist, when the caller could count them — the `of M`
        /// denominator in the note.
        total: Option<usize>,
    },
}

impl DeadCodeRefusal {
    /// The verdict for `repos` against the sidecar at `db_path`, or `None`
    /// when every repo is current.
    ///
    /// The comparison is [`ResolverGenerations::stale_repos`] and nothing
    /// else. nw-358 made that the sole computation behind every route's
    /// staleness answer; a refusal that re-derived it would be the fourth
    /// decider that fix exists to prevent.
    pub fn for_repos(db_path: &Path, repos: &[Repo]) -> Option<Self> {
        if repos.is_empty() {
            return None;
        }
        let stale = load(db_path).stale_repos(repos.iter().map(|repo| repo.uid.as_str()));
        if stale.is_empty() {
            return None;
        }
        let repos_with_remedies = stale
            .into_iter()
            .map(|uid| {
                let path = repos
                    .iter()
                    .find(|repo| repo.uid == uid)
                    .and_then(|repo| repo.local_root())
                    .map(str::to_string);
                StaleRepoRemedy::new(uid, path)
            })
            .collect();
        Some(Self::OutdatedResolver {
            repos: repos_with_remedies,
            total: Some(repos.len()),
        })
    }

    /// Machine-readable cause. `outdated_resolver` is deliberately the SAME
    /// token `stale-check` puts in a repo's `status`, so detection and refusal
    /// share one vocabulary instead of growing two.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::OutdatedResolver { .. } => "outdated_resolver",
        }
    }

    /// The paragraph a human sees, on stderr, on every route.
    pub fn message(&self) -> String {
        self.message_with_preamble(WHY_DEAD_CODE_REFUSES)
    }

    /// The same verdict and the same pasteable remedies, for a caller that is
    /// DEGRADING an edge-dependent analysis rather than refusing `dead-code`.
    ///
    /// nw-419. Both blast-radius wrappers pushed [`Self::message`] verbatim, so
    /// a blast-radius degrade opened by explaining why `dead-code` will not
    /// produce a list -- a command the user did not run, in front of remedies
    /// that were otherwise exactly right. `affected_tests_refusal_payload`
    /// already replaces its `note` for this reason; this is that fix on the
    /// surface that still needed it, shared rather than restated so the two
    /// blast-radius routes cannot drift.
    pub fn edge_analysis_message(&self) -> String {
        match self {
            Self::OutdatedResolver { repos, .. } => {
                // Just the verdict. `incompatibility_message` ends with its own
                // `index --repo <path> --force` TEMPLATE, and
                // `message_with_preamble` appends `staleness_note_for`, which
                // ends with the same template again -- so composing them
                // printed the remedy twice as a run-on before the pasteable
                // command. The preamble states the cause; the remedy block
                // below it states the fix, once.
                let uids: Vec<String> = repos.iter().map(|repo| repo.uid.clone()).collect();
                format!(
                    "edge-dependent analysis cannot trust repositories whose resolver \
                     generation differs from the running generation \
                     {RESOLVER_GENERATION}: {}. Re-index each one:{}",
                    uids.join(", "),
                    self.remedy_lines()
                )
            }
        }
    }

    fn message_with_preamble(&self, preamble: &str) -> String {
        match self {
            Self::OutdatedResolver { repos, total } => {
                let uids: Vec<String> = repos.iter().map(|repo| repo.uid.clone()).collect();
                let note = staleness_note_for(&uids, *total).unwrap_or_default();
                format!("{preamble} {note}{}", self.remedy_lines())
            }
        }
    }

    /// The pasteable remedy block appended under every preamble.
    ///
    /// This sentence existed in THREE places -- here, and hand-copied into
    /// `affected_tests_refusal_payload` in both `src/main.rs` and
    /// `nestweaver-mcp/src/tools.rs`, where each iterated the JSON `remedies`
    /// array to rebuild the identical string. `payload()` serialises `remedies`
    /// straight from `repos`, so iterating the typed rows produces byte-identical
    /// output without the round trip.
    fn remedy_lines(&self) -> String {
        let Self::OutdatedResolver { repos, .. } = self;
        let mut lines = String::new();
        for repo in repos {
            match &repo.command {
                Some(command) => lines.push_str(&format!("\n  {command}")),
                None => lines.push_str(&format!(
                    "\n  {} — indexed from a bare clone, so this machine has no \
                     working tree to pass to `--repo`; re-index it where it lives",
                    repo.uid
                )),
            }
        }
        lines
    }

    /// `affected-tests`' refusal payload, shared by the CLI direct route, the
    /// CLI daemon route and the MCP tool.
    ///
    /// It previously existed as two hand-maintained copies whose only guarantee
    /// of agreement was a doc comment asserting they were "byte-identical" --
    /// including a duplicated copy of the preamble constant itself. A CI gate
    /// must not be able to tell which route answered, so agreement is now
    /// structural.
    pub fn affected_tests_payload(&self) -> serde_json::Value {
        let mut payload = self.payload();
        let note = format!("{WHY_AFFECTED_TESTS_REFUSES}{}", self.remedy_lines());
        payload["note"] = serde_json::json!(note.clone());
        payload["notifications"] = serde_json::json!([{
            "level": "error",
            "descriptor": INCOMPATIBLE_RESOLVER_DESCRIPTOR,
            "message": note,
        }]);
        // The one key a CI consumer acts on. The refusal deliberately carries
        // no tier_1/tier_2/tier_3, so without it a caller keying off "did I get
        // tiers" reads the refusal as "no tests affected" -- the exact silent
        // narrowing this refusal exists to prevent.
        payload["recommendation"] = serde_json::json!("run-full-suite");
        payload
    }

    /// The refusal a MACHINE reads.
    ///
    /// It carries NO `unreachable_symbols` key at all. An empty list would be
    /// the same failure in a politer shape: a caller that keys off "did I get
    /// rows" reads zero rows as "nothing is dead", which is the opposite of
    /// what happened. `refused` is present and `true`, and `reason` says why.
    pub fn payload(&self) -> serde_json::Value {
        let Self::OutdatedResolver { repos, .. } = self;
        serde_json::json!({
            "refused": true,
            "reason": self.reason(),
            "resolver_stale": true,
            "resolver_stale_repos": repos
                .iter()
                .map(|repo| repo.uid.clone())
                .collect::<Vec<_>>(),
            "remedies": repos,
            // The same key `stale-check` gates CI on, so a caller that already
            // reads it needs no edit to see this.
            "needs_reindex": true,
            "note": self.message(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-372. Two properties the route test cannot see from outside, because
    /// its fixture's repo path is a tempdir and its assertions are structural:
    ///
    ///  * the refusal carries NO deletion list — not an empty one. An empty
    ///    array is a claim, and it is the wrong one.
    ///  * the printed command names a REAL path. `staleness_note_for` prints
    ///    the `<path>` template, which is right for the routes that hold no
    ///    repo rows and wrong here, where they are in hand.
    #[test]
    fn a_dead_code_refusal_carries_a_runnable_remedy_and_no_list() {
        let repo = Repo {
            uid: "repo:default:abc".into(),
            url: "file:///tmp/demo".into(),
            indexed_sha: "deadbeef".into(),
            staleness_commits_behind: 0,
            instance_id: "default".into(),
            name: None,
            root_path: Some("/tmp/demo".into()),
        };
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");

        // Current: no refusal at all.
        record(&db, &repo.uid).unwrap();
        assert!(DeadCodeRefusal::for_repos(&db, std::slice::from_ref(&repo)).is_none());

        // Behind: refuse, and name a command that has a real path in it.
        let mut generations = load(&db);
        generations
            .repos
            .insert(repo.uid.clone(), RESOLVER_GENERATION - 1);
        std::fs::write(
            crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR),
            serde_json::to_string(&generations).unwrap(),
        )
        .unwrap();

        let refusal = DeadCodeRefusal::for_repos(&db, std::slice::from_ref(&repo))
            .expect("a repo behind the current generation must refuse");
        let payload = refusal.payload();
        assert_eq!(payload["refused"], serde_json::json!(true));
        assert_eq!(payload["reason"], serde_json::json!("outdated_resolver"));
        assert_eq!(payload["needs_reindex"], serde_json::json!(true));
        assert!(
            payload.get("unreachable_symbols").is_none(),
            "an empty list is a claim, and it is the wrong one: {payload}"
        );
        assert_eq!(
            payload["remedies"][0]["command"],
            serde_json::json!("nestweaver index --repo /tmp/demo --force"),
            "the remedy must be runnable as printed, not the `<path>` template: {payload}"
        );
    }

    #[test]
    fn missing_entry_reads_as_generation_zero() {
        let g = ResolverGenerations::default();
        assert_eq!(g.generation_for("repo:whatever"), 0);
    }

    /// The three preambles share ONE remedy renderer, and the whole safety
    /// claim of that consolidation is that no output text moved. Pin the exact
    /// bytes for both remedy shapes -- a repo with a working tree and a bare
    /// clone without one -- so a future edit to the renderer cannot silently
    /// reword an operator-facing remedy on three surfaces at once.
    #[test]
    fn every_preamble_shares_one_remedy_block_and_none_of_them_moved() {
        let refusal = DeadCodeRefusal::OutdatedResolver {
            repos: vec![
                StaleRepoRemedy::new("repo:worktree".to_string(), Some("/src/a".to_string())),
                StaleRepoRemedy::new("repo:bare".to_string(), None),
            ],
            total: Some(2),
        };

        let expected_remedies = concat!(
            "\n  nestweaver index --repo /src/a --force",
            "\n  repo:bare — indexed from a bare clone, so this machine has no working tree ",
            "to pass to `--repo`; re-index it where it lives",
        );

        // All three surfaces end with the identical block.
        for (label, produced) in [
            ("dead-code", refusal.message()),
            ("edge-analysis", refusal.edge_analysis_message()),
            (
                "affected-tests",
                refusal.affected_tests_payload()["note"]
                    .as_str()
                    .expect("note is a string")
                    .to_string(),
            ),
        ] {
            assert!(
                produced.ends_with(expected_remedies),
                "{label} remedy block moved:\n{produced}"
            );
        }

        // And each still opens with ITS OWN preamble, so sharing the tail did
        // not collapse three different explanations into one.
        assert!(refusal.message().starts_with("dead-code will not produce"));
        assert!(
            refusal.affected_tests_payload()["note"]
                .as_str()
                .unwrap()
                .starts_with("affected-tests will not produce")
        );
        assert!(
            refusal
                .edge_analysis_message()
                .starts_with("edge-dependent analysis cannot trust")
        );

        // The keys a CI consumer acts on survive the move into the engine.
        let payload = refusal.affected_tests_payload();
        assert_eq!(payload["recommendation"], "run-full-suite");
        assert_eq!(payload["needs_reindex"], true);
        assert_eq!(payload["reason"], "outdated_resolver");
        assert_eq!(
            payload["notifications"][0]["descriptor"],
            INCOMPATIBLE_RESOLVER_DESCRIPTOR
        );
        assert_eq!(payload["notifications"][0]["message"], payload["note"]);
        assert!(
            payload.get("tier_1").is_none(),
            "a refusal carries no tiers"
        );
    }

    /// nw-419: `message()` opens with `WHY_DEAD_CODE_REFUSES`, which is the
    /// right sentence for `dead-code` and the wrong one in front of a
    /// blast-radius degrade. Both blast-radius wrappers push it verbatim, so a
    /// user degrading `blast-radius` is told about a command they did not run.
    /// The remedies below it are correct and must survive.
    #[test]
    fn the_edge_analysis_message_drops_dead_codes_sentence_and_keeps_its_remedies() {
        let refusal = DeadCodeRefusal::OutdatedResolver {
            repos: vec![StaleRepoRemedy::new(
                "repo:a".to_string(),
                Some("/src/a".to_string()),
            )],
            total: Some(1),
        };

        // Counterweight: dead-code's own message must KEEP the sentence.
        assert!(
            refusal
                .message()
                .contains("dead-code will not produce a list"),
            "dead-code's own refusal keeps its rationale"
        );

        let edge = refusal.edge_analysis_message();
        assert!(
            !edge.contains("dead-code"),
            "an edge-analysis degrade must not open with dead-code's sentence: {edge}"
        );
        assert!(
            edge.contains("/src/a"),
            "the pasteable remedy must survive: {edge}"
        );
        assert!(
            edge.contains(&RESOLVER_GENERATION.to_string()),
            "the running generation must still be named: {edge}"
        );
    }

    #[test]
    fn only_the_exact_current_generation_is_compatible() {
        let mut g = ResolverGenerations::default();
        g.repos.insert("fresh".into(), RESOLVER_GENERATION);
        g.repos.insert("ancient".into(), 0);
        g.repos.insert("future".into(), RESOLVER_GENERATION + 1);
        let stale = g.stale_repos(vec!["fresh", "future", "ancient", "unrecorded"]);
        assert_eq!(
            stale,
            vec![
                "ancient".to_string(),
                "future".to_string(),
                "unrecorded".to_string(),
            ]
        );
    }

    #[test]
    fn corrupt_and_missing_sidecars_fail_closed_for_known_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let known = ["repo:a"];

        assert_eq!(load(&db).stale_repos(known), vec!["repo:a"]);
        std::fs::write(
            crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR),
            "not-json",
        )
        .unwrap();
        assert_eq!(load(&db).stale_repos(known), vec!["repo:a"]);
    }

    #[test]
    fn all_changed_file_engines_share_the_same_incompatible_repo_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let store = GraphStore::open_or_create(&db).unwrap();
        let repo = Repo {
            uid: "repo:default:future".into(),
            url: "file:///tmp/future".into(),
            indexed_sha: "deadbeef".into(),
            staleness_commits_behind: 0,
            instance_id: "default".into(),
            name: None,
            root_path: Some("/tmp/future".into()),
        };
        store.insert_repo(&repo).unwrap();
        record(&db, &repo.uid).unwrap();

        let files = vec!["src/new.rs".to_string()];
        assert!(
            crate::affected_tests::affected_tests(&store, &files)
                .unwrap()
                .resolver_stale_repos
                .is_empty()
        );

        let mut generations = load(&db);
        generations
            .repos
            .insert(repo.uid.clone(), RESOLVER_GENERATION + 1);
        std::fs::write(
            crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR),
            serde_json::to_string(&generations).unwrap(),
        )
        .unwrap();

        let affected = crate::affected_tests::affected_tests(&store, &files).unwrap();
        assert_eq!(affected.resolver_stale_repos, vec![repo.uid.clone()]);
        assert_eq!(affected.recommendation, "run-full-suite");

        let detected = crate::process::detect_changes_impact(&store, &files, 3).unwrap();
        assert_eq!(detected.resolver_stale_repos, vec![repo.uid.clone()]);
        assert_eq!(
            detected.gate_state,
            crate::blast_radius::GateState::DegradedUnknown
        );

        let blast = crate::blast_radius::analyze_blast_radius(
            &store,
            &[std::path::PathBuf::from("src/new.rs")],
            &crate::blast_radius::BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .unwrap();
        assert_eq!(blast.resolver_stale_repos, vec![repo.uid]);
        assert_eq!(
            blast.gate_state,
            crate::blast_radius::GateState::DegradedUnknown
        );
        for notifications in [
            &affected.notifications,
            &detected.notifications,
            &blast.notifications,
        ] {
            assert!(notifications.iter().any(|notification| {
                notification.descriptor == INCOMPATIBLE_RESOLVER_DESCRIPTOR
            }));
        }
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

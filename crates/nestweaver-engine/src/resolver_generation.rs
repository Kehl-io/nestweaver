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
use std::io::{Read, Write};
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
/// 5 — nw-441: `vue.rs`, `svelte.rs` and `astro.rs` hardcoded
///     `is_entry_point: false` on every symbol and never called
///     `detect_entry_point`. Both `is_entry_point` and `entry_point_kind` are
///     PERSISTED per-symbol columns that `dead-code`, `process.rs` and
///     `ranking.rs` read straight off disk rather than re-deriving, so a
///     component in an already-indexed graph keeps `false` forever and cannot
///     seed a reachability walk no matter which binary asks. Two graphs of
///     identical source then disagree about what is dead, and without this
///     bump the disagreement is silent — the same shape as generation 4's own
///     "svelte/vue/astro named exports change `SymbolKind`" entry, which is
///     the precedent this follows.
/// 6 — nw-435: a Python or bash call/command with no enclosing
///     `function_definition` (and, for Python, no enclosing `lambda`) ancestor
///     now promotes its same-file `Function` callee to `is_entry_point: true`,
///     `entry_point_kind: Some(EntryPointKind::Main)` — see the post-pass in
///     `nestweaver-parser/src/parse.rs`. Before this, `detect_python`/
///     `detect_bash` (entry_points.rs) recognised only fixed names and file
///     patterns — `main`, `handler`/`lambda_handler`, `test_*`, and
///     `views.py`/`routes.py`/`endpoints.py`/`handlers.py` for Python; only
///     `main` for bash — none of which fire for a plain top-level script
///     call, so a bash script or Python module whose top level was bare
///     statements had NO entry point at all and every function it
///     defined was reachability-walked as dead — the dominant real-world bash
///     idiom, and a common Python one. Exactly generation 5's shape again:
///     `is_entry_point`/`entry_point_kind` are PERSISTED per-symbol columns
///     that `dead-code`, `process.rs` and `ranking.rs` read straight off disk
///     rather than re-deriving, so a symbol in an already-indexed graph keeps
///     `is_entry_point: false` forever and cannot seed a reachability walk no
///     matter which binary asks — only re-indexing (`nestweaver index --repo
///     <path> --force`) writes the corrected flag.
///
/// nw-356 (same generation as nw-435 above, per the branch's own
///     coordination note — one bump covers both). Two independent C++
///     `parse.rs` fixes change what gets extracted from `.h`/`.cpp` files.
///     BOTH are gated so an ordinary, already-correct declaration is NEVER
///     touched — an `[[nodiscard]]`- or `static inline`-prefixed multi-line
///     prototype, for example, also has a `declaration` node whose start row
///     precedes its name's row, but is left completely alone. (A) only
///     re-anchors a `declaration`-shaped capture whose reported start row
///     precedes its own `@name` capture's row AND whose subtree contains a
///     tree-sitter `ERROR` node that itself starts strictly BEFORE the
///     `@name` capture's row (`has_error_before_row`) — the signature of an
///     unexpanded macro token sitting where a class name is expected
///     desyncing statement-boundary recovery and widening a LATER, unrelated
///     declaration's span backward across a preceding nested type (the
///     `LBUG_API`/`DataChunkState` witness). An `ERROR` at or after the
///     name's row (e.g. a malformed macro token in the parameter list) does
///     not anchor, since it does not indicate the backward span-widening
///     corruption this fix targets. Symbol UIDs are `(repo, path, name,
///     start_line)`, so ONLY these error-recovered, macro-prefixed
///     declarations get a new UID — a repo indexed before this fix has the
///     OLD (wrong) UID on disk for exactly those declarations, and every edge
///     pointing at one (CALLS, MEMBER_OF) still points at that stale UID
///     after upgrading the binary alone. (B) only reclassifies a directly-
///     initialized local variable whose declaration has no `ERROR` node
///     strictly before the name's row (`has_error_before_row`, not a
///     subtree-wide check — this is what keeps it from colliding with (A)'s
///     witness, whose `ERROR` sits before the name) AND whose type is an
///     inline anonymous/local `struct`/`class` definition (the C++ "most
///     vexing parse": `Type name(initializer);` is grammatically identical to
///     a function declarator). An `ERROR` inside the initializer, after the
///     name, does not block reclassification. Only that exact shape is now
///     correctly classified `SymbolKind::Variable` instead of the spurious
///     `SymbolKind::Function` tree-sitter-cpp's grammar produced — an
///     ordinary function declaration, including one whose call-shaped
///     argument also triggers the most-vexing-parse ambiguity against a
///     plain (non-struct) type (`Foo bar(GetName());`), is unaffected. A
///     `Variable` can never be a CALLS target — exactly generation 5's
///     "svelte/vue/astro named exports change `SymbolKind`" shape and
///     generation 3's `Extension`-vs-`Class` UID-changing reclassification —
///     so this changes which edges an already-indexed repo's stale
///     struct-typed-local symbol can participate in, invisibly to anyone not
///     re-indexing. Same remedy as every other bump in this file: `nestweaver
///     index --repo <path> --force`.
///
/// nw-490 (same generation as nw-435/nw-356 above, per the branch's own
///     coordination note — one bump covers all three; the branch is
///     unreleased). Extends nw-435's exact mechanism (a call/command with no
///     enclosing function body promotes its same-file callee to
///     `is_entry_point: true`, `entry_point_kind: Some(EntryPointKind::Main)`)
///     to Swift, gated to files `main.swift` or a shebang first line — the
///     only Swift files whose top-level statements the compiler executes
///     directly (`@main`-attributed types were already handled by
///     `detect_swift`'s signature check and are untouched). Also widens the
///     promoted-kind gate to `SymbolKind::Class` for Swift only, since a bare
///     top-level call can be an implicit constructor call (`AppDelegate()`),
///     and Swift mints classes/structs/enums/extensions/actors all as
///     `SymbolKind::Class`. Same persisted-column shape as nw-435: a symbol in
///     an already-indexed graph keeps `is_entry_point: false` forever and
///     cannot seed a reachability walk no matter which binary asks — only
///     re-indexing (`nestweaver index --repo <path> --force`) writes the
///     corrected flag.
///
/// nw-491 (same generation as nw-435/nw-356/nw-490 above — the branch is
///     still unreleased). `queries/bash.scm` had no capture for a command's
///     ARGUMENTS, only its own name, so `trap cleanup EXIT` referenced the
///     literal word `trap` and never `cleanup`; the handler had in-degree
///     zero and nw-435's rooting had nothing to promote (its
///     `top_level_called` set comes from `Call` references, and `trap`
///     produced none pointing at the handler). A registration-macro-style
///     post-pass, `collect_bash_trap_targets` in `nestweaver-parser/src/
///     parse.rs`, now parses a bash `trap`'s operands per the GNU Bash
///     manual grammar (`-l`/`-p`/`-P` print or list and register nothing; a
///     leading `--` is consumed; the first remaining operand is the action
///     only when a sigspec also remains; an action of `-` or empty text is a
///     reset/ignore) and promotes the first word of the action text to
///     `is_entry_point: true`, `entry_point_kind:
///     Some(EntryPointKind::EventListener)` on any same-file `Function` of
///     that name — `EventListener`, not `Main`, because a trap handler is
///     triggered by an external OS signal rather than being the script's own
///     entry point, keeping it out of `process.rs`'s `{dir}::main` bucket.
///     Same persisted-column shape as nw-435/nw-490: a symbol in an
///     already-indexed graph keeps `is_entry_point: false` forever and
///     cannot seed a reachability walk no matter which binary asks — only
///     re-indexing (`nestweaver index --repo <path> --force`) writes the
///     corrected flag.
///
/// nw-492 (same generation as nw-435/nw-356/nw-490/nw-491 above — the branch is
/// still unreleased). `parse_manifest` read ONE `package.json`, at the repo
/// root, and only when no other root manifest format matched first, so a
/// monorepo's `packages/*/package.json` — or generated wasm glue declaring
/// `"main": "nestweaver_wasm.js"` under a `Cargo.toml` root — contributed no
/// entry point at all and every symbol it roots was reachability-walked as
/// dead. Entry files are now unioned from EVERY `package.json` in the repo at
/// ANY depth (root included, with or without a `name` field; `node_modules`,
/// the shared skip-dirs and `.gitignore` already applied by the index's own
/// file walk), each raw path is rebased onto the directory containing its own
/// manifest rather than the repo root (an unrebased `"./index.js"` from
/// `packages/a/package.json` names the WRONG file, which is worse than naming
/// none), and `browser` is now an entry file when its value is a string — the
/// object form is a bundler replacement map, not an npm/Node entry point.
/// Same on-disk staleness shape as the rest of this generation, one artefact
/// over: this entry set is computed at INDEX time and persisted in the
/// `<db>.manifests.json` sidecar, and `dead_code`'s reachability walk reads it
/// straight off disk and ORs it with each symbol's persisted `is_entry_point`
/// column to build the same seed set. So a repo indexed before this fix keeps
/// the old root-only, unrebased entry list forever and cannot seed the walk
/// from it no matter which binary asks — only re-indexing (`nestweaver index
/// --repo <path> --force`) rewrites it.
///
/// NOT bumped for nw-453, deliberately. It changes the same persisted
/// `is_entry_point` column in Vue/Svelte/Astro files, but in two directions
/// that each leave a stale graph no worse than the binary that wrote it:
/// (1) route-directory helpers are no longer rooted by the blanket page rule,
/// so a stale graph OVER-roots them and `dead-code` reports LESS dead -- the
/// safe direction, exactly the old output; (2) symbols the component markup
/// uses are newly rooted, so a stale graph still reports template-only
/// handlers outside route directories as dead -- the same false positive the
/// old binary already reported. No persisted row becomes newly wrong, so
/// forcing every user through a `--force` re-index (and making `dead-code`
/// refuse until then) would buy nothing. Re-indexing a repo picks up both.
///
/// 7 — nw-687: 7abe922b adds CALLS/definition edges for CommonJS
///     `module.exports.X = function ...` and `exports.X = function ...`
///     function definitions in JS/TS, which previously produced no `Function`
///     symbol at all. A graph indexed before this fix is missing exactly
///     those symbols AND the edges from their callers to them, so
///     `dead-code`'s reachability walk — which reads persisted symbols and
///     edges straight off disk rather than re-deriving them — reports every
///     live callee reached only through one of these definitions as
///     unreachable. Unlike nw-453 (NOT bumped, above), a stale graph here has
///     no safe reading: the missing symbol and edge make dead code look ALIVE
///     nowhere and make live code look DEAD, the same "previously-indexed
///     repos yield different (wrong) analysis" shape as generations 2 and 4.
///     Same remedy as every other bump in this file: `nestweaver index --repo
///     <path> --force`. Folded into the same bump (review, same PR): those
///     definitions, and a same-file function they bare-identifier re-export
///     (`module.exports.f = f`), are now `Visibility::Public` and root like
///     an equivalent ES export instead of parsing `Private`/non-entry, the
///     same persisted-column shape as generations 5/6 above.
///
/// 8 — nw-688: a JS/TS test-runner block (`describe('getTier', fn)`) was a
///     `Function` named after its title, so it shadowed the real `getTier`:
///     the name went ambiguous and the test's own `getTier()` call bound to
///     the block instead of the definition, hiding the test from
///     `affected-tests`. Blocks are now named `<runner> <title>`
///     (`describe getTier`), which changes their `(repo, path, name,
///     start_line)` UIDs and the source of every CALLS edge inside them. The
///     `require()` import rule now matches `require` only (any
///     `f('string')` call used to be an import), and a call bound from an npm
///     package no longer earns a name-matched `CROSS_REPO_LINK` to another
///     repo's same-named symbol. A stale graph keeps the shadowing symbols and
///     the wrong edges until re-indexed.
pub const RESOLVER_GENERATION: u32 = 8;

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

/// Recovery admission must distinguish missing evidence from unreadable or
/// incompatible evidence. Ordinary query callers retain [`load`]'s fallback.
pub const MAX_RESOLVER_GENERATION_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictResolverGenerations {
    #[serde(deserialize_with = "deserialize_unique_generations")]
    repos: BTreeMap<String, u32>,
}

fn deserialize_unique_generations<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueGenerations;

    impl<'de> serde::de::Visitor<'de> for UniqueGenerations {
        type Value = BTreeMap<String, u32>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a repository generation map without duplicate keys")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut repos = BTreeMap::new();
            while let Some((uid, generation)) = map.next_entry::<String, u32>()? {
                if uid.trim().is_empty() {
                    return Err(serde::de::Error::custom("empty repository UID"));
                }
                if repos.insert(uid, generation).is_some() {
                    return Err(serde::de::Error::custom("duplicate repository UID"));
                }
            }
            Ok(repos)
        }
    }

    deserializer.deserialize_map(UniqueGenerations)
}

/// Strict, bounded sidecar read for a future recovery writer.
///
/// Only a missing path returns `None`. Malformed/unknown shapes, duplicate
/// entries, non-regular files, oversized records and any future generation
/// return an error. A legacy generation (including zero) is evidence to be
/// evaluated by the caller, not permission to migrate it.
pub fn load_strict(db_path: &Path) -> anyhow::Result<Option<ResolverGenerations>> {
    let path = crate::sidecar_path(db_path, RESOLVER_GENERATION_SIDECAR);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect resolver generation sidecar"),
    };
    anyhow::ensure!(
        metadata.is_file(),
        "resolver generation sidecar is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_RESOLVER_GENERATION_BYTES as u64,
        "resolver generation sidecar exceeds byte limit"
    );
    let file = std::fs::File::open(&path).context("open resolver generation sidecar")?;
    let mut bytes = Vec::new();
    file.take(MAX_RESOLVER_GENERATION_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read resolver generation sidecar")?;
    anyhow::ensure!(
        bytes.len() <= MAX_RESOLVER_GENERATION_BYTES,
        "resolver generation sidecar exceeds byte limit"
    );
    let parsed: StrictResolverGenerations =
        serde_json::from_slice(&bytes).context("decode resolver generation sidecar")?;
    anyhow::ensure!(
        parsed
            .repos
            .values()
            .all(|generation| *generation <= RESOLVER_GENERATION),
        "resolver generation sidecar contains a future generation"
    );
    Ok(Some(ResolverGenerations {
        repos: parsed.repos,
    }))
}

/// Strictly merge the current generation into a durably replaced sidecar.
///
/// The caller must hold the database owner's mutation lease across its graph
/// publication and this read/merge/write. This is not a cross-process lock or
/// migration admission check. Missing evidence can be initialized by an
/// explicit successful index; recovery must establish its prerequisites first.
/// Unknown/corrupt/future records are never overwritten. Unrelated entries are
/// preserved. A parent-directory sync failure is an error even though the
/// complete new record may already be canonical; callers must not claim success.
pub fn record_strict(db_path: &Path, repo_uid: &str) -> anyhow::Result<()> {
    record_strict_with_writer(db_path, repo_uid, |file, bytes| file.write_all(bytes))
}

fn record_strict_with_writer(
    db_path: &Path,
    repo_uid: &str,
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !repo_uid.trim().is_empty() && repo_uid.len() <= MAX_RESOLVER_GENERATION_BYTES,
        "invalid repository UID for resolver generation record"
    );
    let mut current = load_strict(db_path)?.unwrap_or_default();
    current
        .repos
        .insert(repo_uid.to_owned(), RESOLVER_GENERATION);
    let bytes = serde_json::to_vec(&current).context("encode resolver generation sidecar")?;
    anyhow::ensure!(
        bytes.len() <= MAX_RESOLVER_GENERATION_BYTES,
        "merged resolver generation sidecar exceeds byte limit"
    );
    let path = crate::sidecar_path(db_path, RESOLVER_GENERATION_SIDECAR);
    nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| write(file, &bytes))
        .context("publish resolver generation sidecar")
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

/// A resolver-generation verdict, split by how the stale repository relates to
/// the change being analysed.
///
/// The first version of this split only changed the MESSAGE: any
/// incompatible repository anywhere still degraded every edge-dependent call,
/// so one stale repository nobody touched made `affected_tests`,
/// `detect_changes` and `blast_radius` answer `run-full-suite` for every
/// change in the graph. A gate that always says that gets ignored, which is
/// the one outcome the gate exists to prevent.
///
/// A stale repository now degrades the answer only when the graph has
/// evidence it can matter:
/// - it OWNS a changed file, or
/// - it could REACH one: it is connected to a repository that owns a changed
///   file through any chain of repo links, each a recorded symbol-to-symbol
///   edge (any `CALLS`/`IMPORTS`/`CROSS_REPO_LINK`/... table) or a declared
///   `[[links]]` entry in the instance config. Either direction counts.
///
/// Anything else is `unrelated`: it is DISCLOSED, by name and with its remedy,
/// and does not degrade the answer. Recorded plus declared links are the
/// reachability evidence the graph holds; a repository with neither has no
/// path into the change that any traversal here could have taken.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolverIncompatibility {
    /// Incompatible repositories that own at least one changed file. The
    /// caller can re-index these themselves.
    pub owning_changed_files: Vec<String>,
    /// Incompatible repositories reachable from an owning repository through
    /// any chain of recorded or declared repo links. These degrade the answer
    /// too.
    pub linked: Vec<String>,
    /// Incompatible repositories with no link to any changed file. Disclosed
    /// only; they do not degrade the answer.
    pub unrelated: Vec<String>,
}

/// Descriptor of the disclosure-only notification naming unrelated stale
/// repositories. Distinct from [`INCOMPATIBLE_RESOLVER_DESCRIPTOR`] so a gate
/// keyed on that descriptor keeps meaning "this answer is degraded".
pub const UNRELATED_STALE_RESOLVER_DESCRIPTOR: &str = "resolver-generation-stale-unrelated";

const REINDEX_REMEDY: &str = "Re-index each with `nestweaver index --repo <path> --force` \
     (`--force` is required: a generation-stale repository is already at HEAD, so the \
     incremental path writes nothing).";

fn repositories(count: usize) -> (&'static str, &'static str) {
    if count == 1 {
        ("repository", "is")
    } else {
        ("repositories", "are")
    }
}

impl ResolverIncompatibility {
    /// Whether the answer is degraded: a stale repository owns or is linked
    /// to a changed file. Unrelated stale repositories do not count.
    pub fn is_incompatible(&self) -> bool {
        !self.owning_changed_files.is_empty() || !self.linked.is_empty()
    }

    /// Every stale repository that degrades this answer, sorted. This is the
    /// population `resolver_stale_repos` carries on the wire, so a consumer
    /// that gates on that field being non-empty gates on the same rule.
    pub fn all(&self) -> Vec<String> {
        let mut all = self.owning_changed_files.clone();
        all.extend(self.linked.iter().cloned());
        all.sort();
        all.dedup();
        all
    }

    /// Keep only the repositories that degrade this answer. The preflights
    /// that refuse or degrade from a `Repo` list share this so they cannot
    /// disagree with the engine about which repositories count.
    pub fn retain_degrading(&self, repos: &mut Vec<Repo>) {
        let degrading = self.all();
        repos.retain(|repo| degrading.binary_search(&repo.uid).is_ok());
    }

    /// The operator-facing explanation of the degrading repositories, or an
    /// empty string when none degrade.
    pub fn message(&self) -> String {
        let owning = &self.owning_changed_files;
        let linked = &self.linked;
        match (owning.is_empty(), linked.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!(
                "The repositories your changed files belong to were indexed by a resolver other \
                 than the running generation {RESOLVER_GENERATION}, so their edges cannot be \
                 trusted and this answer is degraded: {}. {REINDEX_REMEDY}",
                owning.join(", ")
            ),
            (true, false) => {
                let (noun, verb) = repositories(linked.len());
                format!(
                    "Your changed files belong only to repositories built by the running resolver \
                     generation {RESOLVER_GENERATION}, but {} {noun} linked to them by a recorded \
                     cross-repo edge or a declared link {verb} not: {}. An edge written by an \
                     older resolver can be missing, and a missing edge would hide an affected \
                     symbol, so this answer is degraded rather than trusted. {REINDEX_REMEDY}",
                    linked.len(),
                    linked.join(", ")
                )
            }
            (false, false) => {
                let (noun, verb) = repositories(linked.len());
                format!(
                    "The repositories your changed files belong to were indexed by a resolver \
                     other than the running generation {RESOLVER_GENERATION}: {}. {} further \
                     {noun} linked to them by a recorded cross-repo edge or a declared link {verb} \
                     also incompatible and degrade this answer: {}. {REINDEX_REMEDY}",
                    owning.join(", "),
                    linked.len(),
                    linked.join(", ")
                )
            }
        }
    }

    /// The disclosure for unrelated stale repositories, or `None` when there
    /// are none.
    pub fn unrelated_message(&self) -> Option<String> {
        if self.unrelated.is_empty() {
            return None;
        }
        let (noun, verb) = repositories(self.unrelated.len());
        Some(format!(
            "{} {noun} in this graph {verb} indexed by a resolver other than the running \
             generation {RESOLVER_GENERATION}: {}. None of them owns a changed file or shares a \
             recorded cross-repo edge or a declared link with a repository that does, so this \
             answer is not degraded by them. {REINDEX_REMEDY}",
            self.unrelated.len(),
            self.unrelated.join(", ")
        ))
    }

    /// Apply the verdict to an analysis: degrade and explain when a stale
    /// repository owns or reaches the change, and disclose the unrelated ones
    /// without degrading.
    pub fn apply(
        &self,
        status: &mut crate::blast_radius::AnalysisStatus,
        notifications: &mut Vec<crate::blast_radius::Notification>,
    ) {
        use crate::blast_radius::{AnalysisStatus, Notification, NotificationLevel};
        if self.is_incompatible() {
            *status = (*status).max(AnalysisStatus::Degraded);
            notifications.push(Notification {
                level: NotificationLevel::Error,
                message: self.message(),
                descriptor: INCOMPATIBLE_RESOLVER_DESCRIPTOR.to_string(),
            });
        }
        if let Some(message) = self.unrelated_message() {
            notifications.push(Notification {
                level: NotificationLevel::Warning,
                message,
                descriptor: UNRELATED_STALE_RESOLVER_DESCRIPTOR.to_string(),
            });
        }
    }
}

thread_local! {
    /// The instance config whose `[[links]]` count as reachability evidence
    /// for [`incompatibility_for_changed_files`]. Installed by the host for
    /// each dispatch (the MCP crate forwards `set_current_instance_config`
    /// here), because the analysis entry points take no config argument.
    static DECLARED_LINK_CONFIG: std::cell::RefCell<Option<std::sync::Arc<crate::InstanceConfig>>> =
        const { std::cell::RefCell::new(None) };
}

/// Install (or clear, with `None`) the instance config whose declared
/// `[[links]]` this thread's analyses treat as reachability evidence.
pub fn set_declared_link_config(config: Option<std::sync::Arc<crate::InstanceConfig>>) {
    DECLARED_LINK_CONFIG.with(|slot| *slot.borrow_mut() = config);
}

/// Repo-UID pairs joined by a declared `[[links]]` entry. A side that
/// resolves ambiguously contributes every candidate: over-counting a link can
/// only keep a gate degraded, never clear one.
fn declared_link_pairs(repos: &[Repo]) -> Vec<(String, String)> {
    let Some(config) = DECLARED_LINK_CONFIG.with(|slot| slot.borrow().clone()) else {
        return Vec::new();
    };
    let Some(links) = config.links.as_deref() else {
        return Vec::new();
    };
    let side = |declared: &str| -> Vec<String> {
        match crate::project::resolve_declared_repo(declared, &config.repos, repos) {
            crate::project::DeclaredRepoMatch::Ambiguous(candidates) => {
                candidates.iter().map(|repo| repo.uid.clone()).collect()
            }
            found => found
                .attached()
                .iter()
                .map(|repo| repo.uid.clone())
                .collect(),
        }
    };
    let mut pairs = Vec::new();
    for link in links {
        let from = side(&link.from);
        let to = side(&link.to);
        for a in &from {
            for b in &to {
                pairs.push((a.clone(), b.clone()));
            }
        }
    }
    pairs
}

/// The verdict for `changed_files`: which incompatible repositories own a
/// changed file, which are linked to one, and which are unrelated.
///
/// See [`ResolverIncompatibility`] for the rule. The incompatible set itself
/// is still computed over every repository in the store; only whether each
/// one DEGRADES depends on the change.
pub fn incompatibility_for_changed_files(
    store: &GraphStore,
    changed_files: &[String],
) -> anyhow::Result<ResolverIncompatibility> {
    incompatibility_for_changed_files_at(store, store.db_path(), changed_files)
}

/// [`incompatibility_for_changed_files`] against the sidecar of an explicit
/// `db_path`, for hosts that carry the database path beside the store.
/// `None` means no database file, and so nothing incompatible.
pub fn incompatibility_for_changed_files_at(
    store: &GraphStore,
    db_path: Option<&Path>,
    changed_files: &[String],
) -> anyhow::Result<ResolverIncompatibility> {
    let Some(db_path) = db_path else {
        return Ok(ResolverIncompatibility::default());
    };
    let incompatible = load(db_path).stale_repos(
        store
            .list_repos(None)
            .context("list repositories for resolver-generation compatibility")?
            .iter()
            .map(|repo| repo.uid.as_str()),
    );
    if incompatible.is_empty() {
        return Ok(ResolverIncompatibility::default());
    }
    let mut owning: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for file in changed_files {
        // Both the symbols and the File node: a changed file whose symbols
        // were all removed still names the repository that holds it. A file
        // unknown to the graph contributes no owner; it is disclosed as
        // unassessed by the analysis itself. A LOOKUP ERROR propagates: read
        // as "no owner" it would quietly demote a stale owner to unrelated.
        let symbols = store
            .symbols_in_file(file)
            .with_context(|| format!("map changed file {file} to its repository"))?;
        owning.extend(
            symbols
                .into_iter()
                .map(|symbol| symbol.repo_uid)
                .filter(|uid| !uid.is_empty()),
        );
        let repos = store
            .repos_indexing_file(file)
            .with_context(|| format!("map changed file {file} to its repository"))?;
        owning.extend(repos.into_iter().filter(|uid| !uid.is_empty()));
    }
    let (owning_changed_files, rest): (Vec<String>, Vec<String>) = incompatible
        .into_iter()
        .partition(|uid| owning.contains(uid));
    if rest.is_empty() || owning.is_empty() {
        return Ok(ResolverIncompatibility {
            owning_changed_files,
            linked: Vec::new(),
            unrelated: rest,
        });
    }
    let declared = if DECLARED_LINK_CONFIG.with(|slot| slot.borrow().is_some()) {
        let repos = store
            .list_repos(None)
            .context("list repositories for declared links")?;
        declared_link_pairs(&repos)
    } else {
        Vec::new()
    };
    // Every repository reachable from the owning set through ANY chain of
    // repo links: a recorded symbol edge (either direction) or a declared
    // link (either orientation). One hop was not enough: a stale repository
    // that calls B, where B calls the changed repository, can still hide
    // impact through its own missing edges. Breadth-first; the repository
    // count is small, and the walk stops once every stale repository is
    // placed.
    let mut reachable: std::collections::BTreeSet<String> = owning.clone();
    let mut queue: std::collections::VecDeque<String> = owning.iter().cloned().collect();
    while let Some(uid) = queue.pop_front() {
        if rest.iter().all(|stale| reachable.contains(stale)) {
            break;
        }
        // A store error here must not quietly demote a repository to
        // "unrelated": propagate it, the way listing the repositories does.
        let mut neighbours = store
            .repos_sharing_symbol_edges(&uid)
            .with_context(|| format!("list cross-repo edges of {uid}"))?;
        neighbours.extend(declared.iter().filter_map(|(a, b)| {
            if a == &uid {
                Some(b.clone())
            } else if b == &uid {
                Some(a.clone())
            } else {
                None
            }
        }));
        for neighbour in neighbours {
            if reachable.insert(neighbour.clone()) {
                queue.push_back(neighbour);
            }
        }
    }
    let (linked, unrelated): (Vec<String>, Vec<String>) =
        rest.into_iter().partition(|uid| reachable.contains(uid));
    Ok(ResolverIncompatibility {
        owning_changed_files,
        linked,
        unrelated,
    })
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

    /// One graph for every direction of the stale-repo gate: `mine` owns the changed file,
    /// `edge` shares a recorded CROSS_REPO_LINK with `mine`, `declared` is
    /// joined to `mine` only by a declared `[[links]]` entry, and `theirs` has
    /// no link to anything. No repo has a generation record yet, so every one
    /// starts incompatible; each case records the ones it wants current.
    fn nw424_fixture() -> (tempfile::TempDir, std::path::PathBuf, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let store = GraphStore::open_or_create(&db).unwrap();
        let repo = |uid: &str, root: &str| Repo {
            uid: uid.into(),
            url: format!("file://{root}"),
            indexed_sha: "deadbeef".into(),
            staleness_commits_behind: 0,
            instance_id: "default".into(),
            name: None,
            root_path: Some(root.into()),
        };
        let symbol = |uid: &str, repo_uid: &str, file: &str| nestweaver_schema::Symbol {
            uid: uid.to_string(),
            name: uid.to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 2,
            signature: String::new(),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        for name in ["mine", "edge", "declared", "theirs"] {
            let uid = format!("repo:{name}");
            store
                .insert_repo(&repo(&uid, &format!("/src/{name}")))
                .unwrap();
            store
                .insert_symbol(&symbol(
                    &format!("s_{name}"),
                    &uid,
                    &format!("src/{name}.rs"),
                ))
                .unwrap();
        }
        store
            .insert_cross_repo_link("s_edge", "s_mine", 0.9, "http_api")
            .unwrap();
        (dir, db, store)
    }

    fn declared_link_config() -> std::sync::Arc<crate::InstanceConfig> {
        std::sync::Arc::new(
            crate::InstanceConfig::from_toml_str(
                r#"instance_id = "nw424"

[snapshot_storage]
backend = "local"
path = "/tmp"

[workspace]
backend = "local"
path = "/tmp"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "model"
summary_model = "model"

[git]
credential_method = "ssh"

[[links]]
from = "declared"
to = "mine"
type = "http-api"
"#,
            )
            .unwrap(),
        )
    }

    /// Reachability is TRANSITIVE and runs both ways on every link kind.
    /// `top` calls `far`, which calls `mid`, which calls the changed
    /// repository `mine` (three hops); `mine`
    /// calls `rev`; a declared link runs from `mine` to `back`. All four are
    /// reachable from the change, so a stale one degrades; `lone` has no link
    /// and is only disclosed.
    #[test]
    fn a_stale_repo_reachable_through_any_chain_of_links_degrades() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        let store = GraphStore::open_or_create(&db).unwrap();
        for name in ["mine", "mid", "far", "top", "rev", "back", "lone"] {
            let uid = format!("repo:{name}");
            store
                .insert_repo(&Repo {
                    uid: uid.clone(),
                    url: format!("file:///src/{name}"),
                    indexed_sha: "deadbeef".into(),
                    staleness_commits_behind: 0,
                    instance_id: "default".into(),
                    name: None,
                    root_path: Some(format!("/src/{name}")),
                })
                .unwrap();
            store
                .insert_symbol(&nestweaver_schema::Symbol {
                    uid: format!("s_{name}"),
                    name: format!("s_{name}"),
                    kind: nestweaver_schema::SymbolKind::Function,
                    repo_uid: uid,
                    file_path: format!("src/{name}.rs"),
                    start_line: 1,
                    end_line: 2,
                    signature: String::new(),
                    summary: None,
                    content_hash: name.to_string(),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: nestweaver_schema::Visibility::Public,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        for (from, to) in [
            ("s_top", "s_far"),
            ("s_far", "s_mid"),
            ("s_mid", "s_mine"),
            ("s_mine", "s_rev"),
        ] {
            store
                .insert_cross_repo_link(from, to, 0.9, "http_api")
                .unwrap();
        }
        let mut config = (*declared_link_config()).clone();
        for link in config.links.iter_mut().flatten() {
            link.from = "mine".to_string();
            link.to = "back".to_string();
        }
        set_declared_link_config(Some(std::sync::Arc::new(config)));
        record(&db, "repo:mine").unwrap();

        let verdict =
            incompatibility_for_changed_files(&store, &["src/mine.rs".to_string()]).unwrap();
        set_declared_link_config(None);
        assert!(verdict.owning_changed_files.is_empty(), "{verdict:?}");
        assert_eq!(
            verdict.linked,
            ["repo:back", "repo:far", "repo:mid", "repo:rev", "repo:top"].map(String::from),
            "{verdict:?}"
        );
        assert_eq!(verdict.unrelated, vec!["repo:lone".to_string()]);
    }

    /// Both directions on one fixture. An unrelated stale repository
    /// is DISCLOSED and does not degrade; a stale repository that owns a
    /// changed file, or shares a recorded edge or a declared link with one
    /// that does, still degrades.
    #[test]
    fn only_a_stale_repo_that_owns_or_reaches_the_change_degrades() {
        let (_dir, db, store) = nw424_fixture();
        let changed = vec!["src/mine.rs".to_string()];
        set_declared_link_config(Some(declared_link_config()));

        // Everything but `theirs` current: the unrelated one only discloses.
        for uid in ["repo:mine", "repo:edge", "repo:declared"] {
            record(&db, uid).unwrap();
        }
        let verdict = incompatibility_for_changed_files(&store, &changed).unwrap();
        assert_eq!(verdict.unrelated, vec!["repo:theirs".to_string()]);
        assert!(
            !verdict.is_incompatible() && verdict.all().is_empty(),
            "an unrelated stale repository must not degrade the answer: {verdict:?}"
        );
        let disclosure = verdict
            .unrelated_message()
            .expect("the unrelated repository must still be disclosed");
        assert!(disclosure.contains("repo:theirs") && disclosure.contains("--force"));

        // The unrelated repo must be the ONLY thing keeping the answer clean:
        // the three engines see it as a disclosure, not a degrade.
        let affected = crate::affected_tests::affected_tests(&store, &changed).unwrap();
        let detected =
            crate::process::detect_changes_impact(&store, &changed, 3, Some(&db)).unwrap();
        let blast = crate::blast_radius::analyze_blast_radius(
            &store,
            &[std::path::PathBuf::from("src/mine.rs")],
            &crate::blast_radius::BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .unwrap();
        assert!(affected.resolver_stale_repos.is_empty());
        assert!(detected.resolver_stale_repos.is_empty());
        assert!(blast.resolver_stale_repos.is_empty());
        assert_ne!(
            blast.gate_state,
            crate::blast_radius::GateState::DegradedUnknown,
            "{:?}",
            blast.notifications
        );
        for notifications in [
            &affected.notifications,
            &detected.notifications,
            &blast.notifications,
        ] {
            assert!(
                notifications
                    .iter()
                    .any(|n| n.descriptor == UNRELATED_STALE_RESOLVER_DESCRIPTOR),
                "{notifications:?}"
            );
            assert!(
                !notifications
                    .iter()
                    .any(|n| n.descriptor == INCOMPATIBLE_RESOLVER_DESCRIPTOR),
                "{notifications:?}"
            );
        }

        // A recorded cross-repo edge makes a stale repository relevant.
        let _ = std::fs::remove_file(crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR));
        for uid in ["repo:mine", "repo:declared", "repo:theirs"] {
            record(&db, uid).unwrap();
        }
        let verdict = incompatibility_for_changed_files(&store, &changed).unwrap();
        assert_eq!(verdict.linked, vec!["repo:edge".to_string()]);
        assert!(verdict.is_incompatible());
        let blast = crate::blast_radius::analyze_blast_radius(
            &store,
            &[std::path::PathBuf::from("src/mine.rs")],
            &crate::blast_radius::BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .unwrap();
        assert_eq!(blast.resolver_stale_repos, vec!["repo:edge".to_string()]);
        assert_eq!(
            blast.gate_state,
            crate::blast_radius::GateState::DegradedUnknown
        );

        // A declared link does too, and only because of the config: without
        // it the same repository is unrelated.
        let _ = std::fs::remove_file(crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR));
        for uid in ["repo:mine", "repo:edge", "repo:theirs"] {
            record(&db, uid).unwrap();
        }
        let verdict = incompatibility_for_changed_files(&store, &changed).unwrap();
        assert_eq!(verdict.linked, vec!["repo:declared".to_string()]);
        let affected = crate::affected_tests::affected_tests(&store, &changed).unwrap();
        assert_eq!(
            affected.resolver_stale_repos,
            vec!["repo:declared".to_string()]
        );
        assert_eq!(affected.recommendation, "run-full-suite");
        set_declared_link_config(None);
        let verdict = incompatibility_for_changed_files(&store, &changed).unwrap();
        assert_eq!(verdict.unrelated, vec!["repo:declared".to_string()]);
        assert!(!verdict.is_incompatible());

        // The stale repository that owns the changed file degrades.
        let _ = std::fs::remove_file(crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR));
        for uid in ["repo:edge", "repo:declared", "repo:theirs"] {
            record(&db, uid).unwrap();
        }
        let verdict = incompatibility_for_changed_files(&store, &changed).unwrap();
        assert_eq!(verdict.owning_changed_files, vec!["repo:mine".to_string()]);
        let detected =
            crate::process::detect_changes_impact(&store, &changed, 3, Some(&db)).unwrap();
        assert_eq!(detected.resolver_stale_repos, vec!["repo:mine".to_string()]);
        assert_eq!(
            detected.gate_state,
            crate::blast_radius::GateState::DegradedUnknown
        );
    }

    /// The messages name the situation, not just the UIDs.
    #[test]
    fn incompatibility_messages_explain_owning_linked_and_unrelated() {
        let mine_only = ResolverIncompatibility {
            owning_changed_files: vec!["repo:mine".to_string()],
            ..Default::default()
        };
        assert!(mine_only.message().contains("repo:mine"));
        let linked_only = ResolverIncompatibility {
            linked: vec!["repo:edge".to_string()],
            ..Default::default()
        };
        let text = linked_only.message();
        assert!(
            text.contains("repo:edge") && text.contains("declared link"),
            "{text}"
        );
        let unrelated_only = ResolverIncompatibility {
            unrelated: vec!["repo:theirs".to_string()],
            ..Default::default()
        };
        assert!(unrelated_only.message().is_empty());
        assert!(
            unrelated_only
                .unrelated_message()
                .unwrap()
                .contains("not degraded")
        );
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
        // The changed file must belong to the stale repository for it
        // to degrade the answer.
        store
            .insert_file(&nestweaver_schema::File {
                uid: "file:future:src/new.rs".into(),
                path: "src/new.rs".into(),
                repo_uid: repo.uid.clone(),
                content_hash: "h".into(),
            })
            .unwrap();
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

        let detected = crate::process::detect_changes_impact(&store, &files, 3, Some(&db)).unwrap();
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

#[cfg(test)]
mod strict_sidecar_tests {
    use super::*;

    #[test]
    fn strict_missing_and_empty_records_are_distinct_and_can_initialize() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        assert!(load_strict(&db).unwrap().is_none());
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        std::fs::write(&path, br#"{"repos":{}}"#).unwrap();
        assert!(load_strict(&db).unwrap().unwrap().repos.is_empty());
        std::fs::remove_file(&path).unwrap();
        record_strict(&db, "repo-a").unwrap();
        assert_eq!(
            load_strict(&db).unwrap().unwrap().generation_for("repo-a"),
            RESOLVER_GENERATION
        );
        assert!(!db.exists(), "these are sidecar-only tests, with no store");
    }

    #[test]
    fn strict_merge_preserves_unrelated_and_explicit_legacy_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        std::fs::write(&path, br#"{"repos":{"target":1,"other":2,"legacy":0}}"#).unwrap();
        record_strict(&db, "target").unwrap();
        record_strict(&db, "added").unwrap();
        let actual = load_strict(&db).unwrap().unwrap().repos;
        assert_eq!(
            actual,
            BTreeMap::from([
                ("target".to_string(), RESOLVER_GENERATION),
                ("added".to_string(), RESOLVER_GENERATION),
                ("other".to_string(), 2),
                ("legacy".to_string(), 0),
            ])
        );
    }

    #[test]
    fn strict_rejects_bad_shapes_without_overwriting_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        for bytes in [
            b"".as_slice(),
            b"{",
            br#"{}"#,
            br#"{"repos":null}"#,
            br#"{"repos":{"a":-1}}"#,
            br#"{"repos":{"a":"6"}}"#,
            br#"{"repos":{"a":1,"a":2}}"#,
            br#"{"repos":{},"repos":{"a":2}}"#,
            br#"{"repos":{" ":1}}"#,
            br#"{"repos":{},"future_shape":true}"#,
            br#"{"repos":{}} trailing"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(load_strict(&db).is_err(), "accepted {bytes:?}");
            assert!(record_strict(&db, "target").is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn strict_rejects_future_target_or_unrelated_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        for uid in ["target", "other"] {
            let bytes = serde_json::to_vec(&ResolverGenerations {
                repos: BTreeMap::from([(uid.to_string(), RESOLVER_GENERATION + 1)]),
            })
            .unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(
                load_strict(&db)
                    .unwrap_err()
                    .to_string()
                    .contains("future generation")
            );
            assert!(record_strict(&db, "target").is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(load(&db).generation_for(uid), RESOLVER_GENERATION + 1);
        }
    }

    #[test]
    fn strict_bounds_input_and_merged_output_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        let oversized = vec![b' '; MAX_RESOLVER_GENERATION_BYTES + 1];
        std::fs::write(&path, &oversized).unwrap();
        assert!(
            load_strict(&db)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert!(record_strict(&db, "target").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), oversized);

        let original = br#"{"repos":{"other":2}}"#;
        std::fs::write(&path, original).unwrap();
        let oversized_uid = "a".repeat(MAX_RESOLVER_GENERATION_BYTES);
        assert!(record_strict(&db, &oversized_uid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(record_strict(&db, " ").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn strict_io_failure_is_not_missing_and_query_fallback_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        std::fs::create_dir(&path).unwrap();
        assert!(load_strict(&db).is_err());
        assert!(record_strict(&db, "target").is_err());
        assert!(path.is_dir());
        assert!(load(&db).repos.is_empty());
    }

    #[test]
    fn strict_partial_temp_write_failure_preserves_old_record_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let path = crate::sidecar_path(&db, RESOLVER_GENERATION_SIDECAR);
        let original = br#"{"repos":{"target":1,"other":2}}"#;
        std::fs::write(&path, original).unwrap();
        let error = record_strict_with_writer(&db, "target", |file, bytes| {
            file.write_all(&bytes[..bytes.len() / 2])?;
            Err(std::io::Error::other("injected partial write failure"))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected partial write failure"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(
            load_strict(&db).unwrap().unwrap().generation_for("target"),
            1
        );
        record_strict(&db, "target").unwrap();
        assert_eq!(
            load_strict(&db).unwrap().unwrap().generation_for("other"),
            2
        );
    }
}

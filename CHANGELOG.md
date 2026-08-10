# Changelog

## Unreleased

### ⚠ BREAKING CHANGES

* **daemon/config:** commands that explicitly pass `--config` now fail when a current-version, already-running daemon cannot prove the same canonical configured path (including compiled-default daemons and daemons using a different config). Previously these commands could exit successfully while silently ignoring the requested configuration. Use `nestweaver daemon --db <db> restart --config <path>` to apply a different configuration. A verified version-upgrade restart still applies the explicit config to its verified successor. Relative paths and symlinks remain accepted when they canonicalize to the daemon's configured path. Editing that file in place does not trigger a path mismatch, but the daemon must still be restarted to load the changed contents.

### Bug Fixes

* **search:** `regex-search`/`count-patterns` — fix trigram pre-filter correctness for alternation patterns; detect a stale trigram index and fall back to a full scan (`stale_index: true` in JSON, note in text output; remedy: re-run `index --with-trigrams`); bound `--limit` to 1–10000 and `--max-millis` to 1–600000
* **impact/trust:** `impact`/`brain_impact` fail closed on unknown or foreign UIDs (CLI exit 2, MCP `not_found`) instead of returning an empty result; `--depth` bounded to 1–15 on CLI and MCP (`pr-impact --depth` same)
* **impact/trust:** `affected-tests` — changed files with unrecognized extensions (Makefile, CI YAML, `.sql`, `.proto`, …) now report `status: partial` with `recommendation: run-full-suite` instead of a silent `complete`; markdown-only changes stay `complete`
* **impact/trust:** `dead-code`/`dead_code` JSON gains `matching_count` (post-`--min-confidence`); `unreachable_count` is again the unfiltered total, consistent with `total_symbols`/`reachable_symbols`/`dead_percentage`; `--limit` bounded to 1–1000
* **MCP validation:** enforce `--tools`/`--lite` allowlists on all transports (local stdio, daemon proxy, hybrid, MCP-over-HTTP); add numeric bounds to tool schemas (`token_budget` 1–16000, `depth`/`max_depth` 1–15, `limit` 1–1000, `include_neighbors` 0–255); hardened tools reject unknown argument names instead of silently ignoring them; `regex_search` rejects conflicting `pattern`+`query` and invalid `kinds`; stdio tool errors no longer carry a misleading `gRPC error:` prefix; `brain_guide` via daemon errors if `config` is passed (use CLI `generate-guide --config`)
* **investigate:** the query's own seed/direct-hit nodes appear first in the map with `is_seed: true`; `--scope` is validated strictly (`project:<name>`, `repo:<name>`, `vault`, `all`; unknown scopes error); `project:` scope filters symbols to project members; bundle storage is concurrency-safe and salvages corrupt sidecars; hydrate skips over-budget bodies instead of aborting
* **daemon/watch:** `watch` works without an instance config (daemon-side watcher with a safe-root denylist); `--force` replaces an existing watcher; direct fallback only when no daemon is running; lock errors hint at `daemon stop`
* **daemon/watch:** `daemon stop`/`status` verify the pidfile PID belongs to this DB's daemon (cmdline + socket peer-PID cross-check) and refuse to signal foreign processes; `daemon start` waits for real health (up to 60s) with a "still booting" message instead of a false failure, and refuses to install over a not-yet-exited old daemon; `ui` releases its port on Ctrl-C and errors clearly on an already-bound port
* **CLI correctness:** `stale-check` exits 1 when any repo is stale (CI freshness gate), flags deleted working trees as `[missing]`, and reports an accurate `stale_repos`
* **CLI correctness:** `clusters`/`cluster` docs and output correctly describe the algorithm as Louvain-style local moving (single-level, not full Leiden) with true Newman–Girvan modularity; non-finite/non-positive `--resolution` clamps to 1.0; `cluster <id|name>` reads the cached sidecar
* **CLI correctness:** `export` — cypher/graphml/mermaid carry real PageRank scores and mermaid `--top N` ranks by actual PageRank; msgpack always writes to `--output`/`<db>.graph.msgpack`, never stdout
* **CLI correctness:** `contracts diff` content-sniffs explicit spec paths (any filename works for OpenAPI JSON/YAML); `generate-guide --format` is validated; `materialize-projects` rejects duplicate project names in the instance config (exit 1); `index --repo` rejects non-directory paths (exit 1, naming the path); `embed` reports a clear "stop the daemon first" error when a daemon holds the DB lock and requires `--batch-size` ≥ 1; `server init-tls --validity-days` bounded to 1–36500 and re-running over an existing CA warns about invalidating signed certs
* **CLI correctness:** read/lookup commands (`list-repos`, `stale-check`, `prune-stale`, `remove-repo`, `export`, brain/memory reads, `hubs`, `instance merge`/`remove --purge-graph`) fail with `db_not_found` (exit 1) against a missing database instead of creating an empty DB and reporting success
* **CLI correctness:** `pull --ephemeral` uses a unique temp workspace (never deletes a persistent checkout) and failed pulls clean up after themselves (only destinations the pull created); `instance abort-migration --force` can discard an unreadable/corrupt migration journal (phase unknown — reconcile manually); `rts-eval record-truth` re-recording the same (repo, sha) upserts (correction wins) instead of first-record-wins
* **docs:** README, CI guide, instance-config and instance-migration guides, server-mode doc, and integration READMEs updated to match the above
* **daemon/watch (critical):** `watch` reindex no longer drops edges — a watcher-processed file previously lost every incoming/outgoing CALLS/IMPORTS edge (single-file resolution only) until a full `--force` reindex; the watcher now shares the incremental indexer's reverse-dependent re-resolution. A transient unreadable file no longer kills the watcher thread and deletes the file's symbols; per-file failures are skipped, never fatal
* **daemon/watch:** watcher embedding is debounced (30s) with a circuit breaker after repeated all-fail passes — fixes a 300%+ CPU hot loop and multi-minute reindex bursts on save-heavy workflows
* **resolver:** Rust cross-crate CALLS/imports now resolve across workspace crates (`crate::`/`super::`, named workspace crates, `tests/` against the crate-under-test, `use` lists/wildcards/aliases) — blast radius for library-crate symbols was massively understated; Python module-level code (`if __name__ == "__main__":`) is no longer attributed to the preceding `def`, eliminating phantom call edges
* **impact/trust:** `impact` reports when traversal was pruned by the impact-score threshold or max depth (text note + honest JSON object on the CLI, `truncated_by_threshold`/`truncated_by_depth` on MCP `brain_impact`) and gains `--min-score` to opt out — reported impact is disclosed as a floor instead of silently incomplete
* **impact/trust:** `dead-code` no longer flags Rust `pub mod` Module symbols (the crate's public API) as unreachable; the visibility-not-persisted limitation (all confidence scores are inferred/Medium) is documented
* **perf:** `brain_context` skips the BERT semantic leg entirely when the database has no embedding vectors, and identical concurrent calls coalesce single-flight — eliminates 30s stampede timeouts under concurrency; trigram index builds are transactional (20+ min → ~30s on a 15k-symbol repo)
* **server:** admin tokens bypass the `/mcp` per-session rate limiter (parity with the gRPC interceptor); `init-tls` certificates carry AKI/BasicConstraints/KeyUsage so strict verifiers (Python 3.13+) accept them; `ui` reports the actual running port (no dead URL on a second request) and exits non-zero on a busy port
* **misc:** `summary --target` matches symbol name/path/exact-UID instead of UID hex substrings; `/api/v1/llm/query` strips punctuation from keyword seeds; `context`/`brain context`/`brain search` CLI bounds match the MCP schemas; rts-eval joins truths 1:1 to the temporally-correct selection, locks its sidecars against concurrent recorders, rotates history atomically, and marks recall estimates uncertain (not an upper bound) when failures are unconfirmed
* **rts-eval:** the report cache (which feeds the in-band `measured` disclosure) refreshes from any report covering all joined data — the default `--window 50` no longer freezes once history exceeds 50, and a genuine slice still never clobbers the canonical report; the temporal join picks the greatest eligible timestamp (parallel recorders can append out of order) and never pairs a truth with a future-dated selection; the sidecar lock writes an ownership token so a stale-lock takeover can't void the successor's mutual exclusion
* **perf:** `brain_context`'s no-embeddings probe is time-bounded (30s) — embeddings added through the daemon without a graph mutation are picked up instead of being hidden behind a cached negative
* **daemon/watch:** a mid-mutation reindex failure (delete landed, re-add failed) is reported as failed after publication finalizes instead of being misreported as "previous data preserved"; the watcher embed pass budgets by attempts, not successes, so a failing endpoint can't fire 3x the intended requests per pass
* **resolver:** Rust `use ... as` aliases become local bindings (calls through aliased imports resolve; same-file shadowing wins); the root-crate fallback is restricted to dev targets (tests/benches/examples) and runs after unique-crate resolution, so external crates like `serde::de` no longer bind to local modules; builtin crates (`std`/`core`/`alloc`) never resolve to local modules; regex-parsed languages (Astro/Svelte/Vue/Cobol) keep intra-function call edges via a degenerate-span (start == end) fallback in enclosing-symbol attribution
* **store:** `purge_instance`/`merge_instance_ids` legacy wrappers route through `legacy_mutation_result` (partial/ambiguous outcomes are no longer reported as success); the merge final probe excludes intentional collision discards, so a clean target-wins merge verifies Applied
* **server:** mTLS client certificates carry AKI/BasicConstraints/KeyUsage like the CA/server certs
* **investigate:** the bundle storage lock gains the ownership token + post-create verify (a stale-lock takeover can't void a successor's lock)
* **daemon/ui:** an already-running `ui` now starts a requested `--watch` session instead of silently dropping it
* **daemon:** `daemon stop` against a dead pidfile PID retargets the live socket-peer daemon instead of declaring not-running and deleting its socket; `wait_healthy` caps each health-check attempt at the remaining budget so the poll loop never overshoots its timeout
* **CLI correctness:** `impact --confidence` forces the direct path (the daemon tool hardcodes 0.0 and would silently ignore the filter); the daemon impact path renders the truncation flags/note in `--json` and text; `brain search --limit` is capped at 1000 to match the MCP schema; `rts-eval --sha` no longer panics on multibyte input
* **MCP parity:** `brain_add_source` applies the directory-name default to vaults only — code repos keep the empty name so the daemon's package/remote derivation still runs; `brain_search` note rows carry `vault_uid` on all paths (BM25, substring fallback, federation) and symbol rows omit empty `matched_headings`

## [4.1.1](https://github.com/Kehl-io/nestweaver/compare/v4.1.0...v4.1.1) (2026-08-10)


### Bug Fixes

* **ci:** satisfy integrated clippy checks ([1a8116f](https://github.com/Kehl-io/nestweaver/commit/1a8116ffb9f2790a3243586622e542706b6048e4))
* **cli:** close MCP and vault refresh regressions ([7793f5a](https://github.com/Kehl-io/nestweaver/commit/7793f5a18f82e73190d0355d1b9451f0adb0a207))
* **cli:** honor persisted intent on daemon fallback ([2b10d2a](https://github.com/Kehl-io/nestweaver/commit/2b10d2a88b54323b4055369765bec79af15ea0f0))
* close lifecycle and selector regressions ([c605ceb](https://github.com/Kehl-io/nestweaver/commit/c605cebdb33d330291505708675855180cc41538))
* close strict preflight and startup budget gaps ([b62a326](https://github.com/Kehl-io/nestweaver/commit/b62a326152e40dd64c470735d3884ee46ea9a554))
* close validated v4.1.0 end-to-end regressions ([e7950b1](https://github.com/Kehl-io/nestweaver/commit/e7950b156fd453756bc04073e0eb1dab03528756))
* **daemon:** finish default reset after slow start ([97f5803](https://github.com/Kehl-io/nestweaver/commit/97f5803924d2a40cac11fae04840d00594b29f96))
* **engine:** harden contract derivation identity ([aa17b85](https://github.com/Kehl-io/nestweaver/commit/aa17b85dd5d93043d2edb3d0caba1dc35bf8ca23))
* finalize integrated regression remediation ([04b9e75](https://github.com/Kehl-io/nestweaver/commit/04b9e75998ad98a3b6df9058e8d5605a1bb8ccaf))
* **graph:** preserve project and contract API identity ([6e9ce98](https://github.com/Kehl-io/nestweaver/commit/6e9ce98252b62b8066857d28f0d915b3b6cbfaa6))
* preserve daemon config across cold starts ([ab7a384](https://github.com/Kehl-io/nestweaver/commit/ab7a384d02ec044222ce25b9ae34b0170c9a32e9))
* **snapshot:** harden restore and embedding consistency ([ac8aaaa](https://github.com/Kehl-io/nestweaver/commit/ac8aaaa0fa413b60e386d4bef2ea908da07c156c))
* **snapshot:** preserve complete embedding state atomically ([928b3da](https://github.com/Kehl-io/nestweaver/commit/928b3dafcf1fb147eb9d6734bd712e518735076e))
* **snapshot:** scope reader capability fallback ([11191f3](https://github.com/Kehl-io/nestweaver/commit/11191f3f1e89737ab075ff73f84e29dcdbf45450))
* **store:** ignore orphan contract debt markers ([dff5552](https://github.com/Kehl-io/nestweaver/commit/dff5552a556710890a58be834297598728e6c6b6))
* **vault:** commit incremental refresh as one batch ([fb8a97c](https://github.com/Kehl-io/nestweaver/commit/fb8a97cf464c3691f2025283aa01ea764ccf6be5))
* **vault:** make incremental refresh replacements atomic ([5eaa926](https://github.com/Kehl-io/nestweaver/commit/5eaa926b790155de3441185a51f018a2413d5a1a))
* **vault:** preserve incremental resolver parity ([810071f](https://github.com/Kehl-io/nestweaver/commit/810071fdfca18e09dd1c071bf7d90c7218e1e40e))
* **vault:** remove newly ignored notes on refresh ([ccfa485](https://github.com/Kehl-io/nestweaver/commit/ccfa485db0169021d2dc4bc1c8dc1cade0a50f17))
* **vault:** scope unresolved refresh dependencies ([0323f5c](https://github.com/Kehl-io/nestweaver/commit/0323f5cb0e80753a34446e4374486983adcb37b6))

## [4.1.0](https://github.com/Kehl-io/nestweaver/compare/v4.0.0...v4.1.0) (2026-08-09)


### Features

* **cli:** report effective daemon config ([f5d79cb](https://github.com/Kehl-io/nestweaver/commit/f5d79cb9cd70d41366616c99b33ac2b30157071f))
* **daemon:** detect and report models in the legacy cache directory ([271b59a](https://github.com/Kehl-io/nestweaver/commit/271b59ac85a1076347144fe7ea8f785d18615482))
* **daemon:** expose effective config provenance ([dad38d6](https://github.com/Kehl-io/nestweaver/commit/dad38d66d839c2e186de97b58a4495e48233c1fd))
* **daemon:** persist live config provenance ([d1c7fdc](https://github.com/Kehl-io/nestweaver/commit/d1c7fdcbbd1d64e735b680f94f3efd91010e4ea4))
* **daemon:** ready-path embedding diagnostic names model and cache dir ([5bfb654](https://github.com/Kehl-io/nestweaver/commit/5bfb65407f4d3f039f6e3c1f481afffdb9fb4d96))
* **daemon:** report a committed-after-cancel index and name index --force ([4a319eb](https://github.com/Kehl-io/nestweaver/commit/4a319eb201c5fa10cfb3da6e8406cfbe47bb833a))
* **engine:** poll cancellation at the pre-write boundary ([d4486af](https://github.com/Kehl-io/nestweaver/commit/d4486af2997279447250dab57f994ce317457dae))
* **engine:** report contract derivation status honestly ([8e13c29](https://github.com/Kehl-io/nestweaver/commit/8e13c29336d90ef6b3754380e7cefb646c94aacb))
* **store:** add repo-wide resolved-edge delete ([f63c399](https://github.com/Kehl-io/nestweaver/commit/f63c3995469571434470324843e0d53f27c3f32d))


### Bug Fixes

* **brain:** report committed refresh deletions ([021958a](https://github.com/Kehl-io/nestweaver/commit/021958a498bb995700f22baf3f7c10d1be6fd041))
* **ci:** satisfy engine clippy checks ([791ebe5](https://github.com/Kehl-io/nestweaver/commit/791ebe519cb10108bd94955a5885761637ac6eac))
* **cli:** checkpoint the embedding index during long passes ([6f2275c](https://github.com/Kehl-io/nestweaver/commit/6f2275c1301d0322eb5b1b78e947ec32d7acf2e3))
* **cli:** embedding metadata truth — gated stamping, fingerprint reads, model guard, checkpointing ([0b8a0ec](https://github.com/Kehl-io/nestweaver/commit/0b8a0ec68eb7d894696417575cd4bee7d6d6a1d0))
* **client:** preserve config across version restarts ([2c71682](https://github.com/Kehl-io/nestweaver/commit/2c71682816bb3b6c8629ef00ce8db712b1d016a1))
* **client:** satisfy clippy path handling ([44eff5e](https://github.com/Kehl-io/nestweaver/commit/44eff5e3e2bc13a31af14d40744db594b950a425))
* **cli:** handle closed stdout quietly ([5a471af](https://github.com/Kehl-io/nestweaver/commit/5a471af5de0469c1345dac994626e518cb67b5e9))
* **cli:** preserve config across daemon restarts ([82c0dce](https://github.com/Kehl-io/nestweaver/commit/82c0dce96186fac3f0b47df0d32890021e8a3886))
* **cli:** read the recorded embedding fingerprint on direct embed paths ([3eed803](https://github.com/Kehl-io/nestweaver/commit/3eed8032fc811420f27a9c472fad940c41818fe7))
* **cli:** reject explicit config fallback ([197e260](https://github.com/Kehl-io/nestweaver/commit/197e260b439a5700b0221ff1328f58992d1eface))
* **cli:** stamp embedding metadata only from vectors the run produced ([a0f1784](https://github.com/Kehl-io/nestweaver/commit/a0f17845155a27e07a2c7249850a2fb149f73ace))
* **cli:** stamp the fingerprint at embed checkpoints mid-run ([a1ed98a](https://github.com/Kehl-io/nestweaver/commit/a1ed98adeaf76a717269df94808b87f647833e7b))
* close end-to-end CLI, daemon, watcher, and MCP regressions ([4a84444](https://github.com/Kehl-io/nestweaver/commit/4a8444444dde92174ea3421411f7664878f1904a))
* **contracts:** route list through daemon rpc ([06ca266](https://github.com/Kehl-io/nestweaver/commit/06ca2661874d82130bbac02d33090168b1213d63))
* **daemon:** cancelled-index honesty — non-terminal timeout warning, committed-after-cancel reporting, pre-write poll ([3b9fb9c](https://github.com/Kehl-io/nestweaver/commit/3b9fb9c9669542eb3f0f3ca88eda563056ec75fa))
* **daemon:** enforce explicit config identity ([15077f8](https://github.com/Kehl-io/nestweaver/commit/15077f87cc4efdbc53e46d70a49ca88d1b6d0a19))
* **daemon:** harden embedding cache diagnostics ([ca868b6](https://github.com/Kehl-io/nestweaver/commit/ca868b6c060182c531ca8f7ced8f58b2e0bd4f2d))
* **daemon:** isolate external embeddings from local cache ([5c5116b](https://github.com/Kehl-io/nestweaver/commit/5c5116b398b7fdca629b4c5ce789a924fc30d992))
* **daemon:** preserve and enforce effective config ([893e08d](https://github.com/Kehl-io/nestweaver/commit/893e08d9d64ee3008c1606c0ad90c629dca25ff6))
* **daemon:** reject unhonored explicit config ([f50d77d](https://github.com/Kehl-io/nestweaver/commit/f50d77d8187609e71050bdcdd298aa006a4a0596))
* **daemon:** report index timeout as a non-terminal stream warning ([7568b69](https://github.com/Kehl-io/nestweaver/commit/7568b690d9eed8df3b365ad62346eeb7645fd01c))
* **daemon:** send the terminal Done before skipping post-index phases on cancel ([2a8cc61](https://github.com/Kehl-io/nestweaver/commit/2a8cc61cdb86f367dc466e6b5f61b3c7116c3bca))
* **daemon:** stamp the fingerprint at embed RPC checkpoints mid-run ([5e23166](https://github.com/Kehl-io/nestweaver/commit/5e23166790637466436b2664ba113d9f4dafd231))
* **engine:** bump in-memory generation on a cancelled commit and drop a dead scope ([724bbb4](https://github.com/Kehl-io/nestweaver/commit/724bbb4273f53ad2cd92531cc753b630a0b0f4ab))
* **engine:** clear stale resolved edges whenever full resolution runs ([08cf08b](https://github.com/Kehl-io/nestweaver/commit/08cf08bfdad2d854fe07b7cc981e55d567b2563c))
* **engine:** contract derivation integrity — CSV pinning, UID dedup, atomicity, honest status ([6d04221](https://github.com/Kehl-io/nestweaver/commit/6d0422138edde0a47844723255ef851bd2f1fcf0))
* **engine:** deduplicate contract UIDs before the bulk COPY ([411b778](https://github.com/Kehl-io/nestweaver/commit/411b77854d2b146871c3ed0cc407d7865ceed8f1))
* **engine:** don't clobber dep records of files not actually re-resolved ([91a5fa3](https://github.com/Kehl-io/nestweaver/commit/91a5fa370dd27de56fb82a247351aec96eea4f23))
* **engine:** force full replacement when resolution deps are empty for the repo ([5a7c473](https://github.com/Kehl-io/nestweaver/commit/5a7c4734c7f213d115b180fea7120d1e578eb939))
* **engine:** keep .index-dirty and the generation counter dirty on a cancelled commit ([f33b4fc](https://github.com/Kehl-io/nestweaver/commit/f33b4fc671ed36f7fc9c4649acdb97e6499b7110))
* **engine:** make contract derivation atomic ([7c3b82a](https://github.com/Kehl-io/nestweaver/commit/7c3b82ac65cfa03f524795afab8f1f21416cde54))
* **engine:** platform-native embedding cache-dir default ([864d8ef](https://github.com/Kehl-io/nestweaver/commit/864d8ef03a5762f101e6ca249bfd3767ab3308c7))
* **engine:** stop duplicate resolved-edge accumulation on empty resolution-deps sidecar ([899e7cf](https://github.com/Kehl-io/nestweaver/commit/899e7cfe41632448419ec0affe638c89e4d051c0))
* **index:** refresh contracts during incremental indexing ([ca42d6c](https://github.com/Kehl-io/nestweaver/commit/ca42d6c5b543e07010789cfb0df41242ea64df12))
* **mcp:** stop reporting a degraded repo as clean ([4f9f067](https://github.com/Kehl-io/nestweaver/commit/4f9f067591ce3491575956fc7c1a9e52c1c07059))
* **mcp:** validate json-rpc envelopes ([f1bdfc9](https://github.com/Kehl-io/nestweaver/commit/f1bdfc9ff15387c1afaeb9bda7fe4c0ac6e56bec))
* **storage:** refuse same-dimension writes from a different recorded model ([0d67df4](https://github.com/Kehl-io/nestweaver/commit/0d67df46dc98b49ea877df74adc874061cc8b3e3))
* **storage:** stamp the embedding fingerprint with each checkpoint flush ([419de92](https://github.com/Kehl-io/nestweaver/commit/419de92a991de46961ceddf05823a9b64b63c2f4))
* **store:** pin the CSV dialect on every COPY ([553bd6d](https://github.com/Kehl-io/nestweaver/commit/553bd6d6c443abcbf24bf458e7d137131fc7a376))
* **watch:** publish contract refreshes atomically ([8f2be7d](https://github.com/Kehl-io/nestweaver/commit/8f2be7d8c171a797ed8e4bcc900a0bae8146cf1b))


### Performance Improvements

* **store:** add remove-repo hub-degree regression benchmark ([27afb9f](https://github.com/Kehl-io/nestweaver/commit/27afb9f7b7d36b21e75b0463f6e6cdff05316379))
* **store:** add remove-repo hub-degree regression benchmark ([42a46d6](https://github.com/Kehl-io/nestweaver/commit/42a46d6ff8e9ec09b2d726752996bd1c0a59f2a5))

## [4.0.0](https://github.com/Kehl-io/nestweaver/compare/v3.0.0...v4.0.0) (2026-08-06)


### ⚠ BREAKING CHANGES

* **deps:** the on-disk storage format moves from version 42 to 43. Databases written by earlier releases (lbug 0.18.2) are still READ by this release without a re-index, but the migration is one-way and is applied automatically: the daemon rewrites the database to v43 on its shutdown checkpoint. Because the CLI auto-spawns a daemon, even a read-only command such as `search` or `list-repos` will upgrade the file once that daemon exits. After the upgrade, older builds fail to open the database with "Trying to read a database file with a different version. Database file version: 43, Current build storage version: 42".

### Bug Fixes

* **deps:** return lbug to upstream 0.19.1 ([7919574](https://github.com/Kehl-io/nestweaver/commit/7919574c6f016b5b418fd9a5e7869c2376c09b7a))

## [3.0.0](https://github.com/Kehl-io/nestweaver/compare/v2.7.1...v3.0.0) (2026-07-31)


### Upgrade Notes

* **Re-index your repositories after upgrading.** The import-edge fan-out fix
  (nw-103) changed how edges are *written*, not how they are read, so it runs at
  index time and cannot repair edges already in your database. Until a repo is
  re-indexed, `hubs`, `bridges`, `summary --level hub` and PageRank keep
  returning the pre-fix ranking for it — on a real 34-repo graph the upgraded
  binary still reported the exact corrupted ranking from the bug report, and
  re-indexing a single repo removed all of its artefacts from the top 10.

  ```sh
  nestweaver index --repo /path/to/each/repo
  ```

  You do not have to guess whether this affects you: `hubs` and `bridges` now
  report on stderr when a database contains repos indexed by an older resolver,
  and stay quiet once everything is current. Vault (`brain refresh`) data is
  unaffected — this applies to code repositories only.

### ⚠ BREAKING CHANGES

* **cli:** one JSON envelope for impact, and dead-code parity across paths ([#218](https://github.com/Kehl-io/nestweaver/issues/218))

  **`impact --json` / `brain_impact`.** Previously emitted three incompatible
  shapes: a bare node array for a complete walk, an object when the traversal was
  pruned, and a bare *candidate* array when the symbol name was ambiguous. The
  last was the dangerous one — structurally indistinguishable from a result set,
  so a mistyped name returned candidates that read as impacted symbols, and only
  the exit code disambiguated. Piped JSON never sees an exit code.

  Every outcome now returns one envelope discriminated by `status`
  (`ok` / `ambiguous` / `not_found`). The ambiguous form carries `candidates` and
  never `nodes`, so the two cannot be confused.

  ```jsonc
  // before (complete walk)        // after
  [ { "uid": "...", ... } ]        { "status": "ok", "nodes": [ { "uid": "...", ... } ] }
  ```

  *Migration:* read `status` first, then `nodes` (or `candidates`). A consumer
  that indexes the top-level value as an array must be updated.

  **`dead-code --json`.** `confidence` serialised as the PascalCase variant name
  on the direct path (`"Medium"`) and lowercase through the daemon (`"medium"`),
  so output depended on whether a daemon happened to be running. Lowercase wins —
  it already matches `Display`, the daemon, and the `--min-confidence` values
  callers pass in. The direct path now also reports its own local scope in `_meta`
  rather than omitting the field.

### Features

* **cli:** add blast-radius and flow-trace subcommands ([#219](https://github.com/Kehl-io/nestweaver/issues/219)) ([1133382](https://github.com/Kehl-io/nestweaver/commit/1133382c8a63afbdbe8d821a1c156e41e165f0f6))
* **cli:** detect nw-073 crash recurrence in diagnostics capabilities ([#200](https://github.com/Kehl-io/nestweaver/issues/200)) ([72b47d2](https://github.com/Kehl-io/nestweaver/commit/72b47d295d6193433eab231b6db6bd7c5c49de9c))


### Bug Fixes

* **client:** wait for a booting daemon, fail fast on a dead one ([#210](https://github.com/Kehl-io/nestweaver/issues/210)) ([7d82ac7](https://github.com/Kehl-io/nestweaver/commit/7d82ac71d0c6f17055b57ea4df1b4054e57e5a16))
* **cli:** make dead-code and investigate output format depend on the flag, not the daemon ([#208](https://github.com/Kehl-io/nestweaver/issues/208)) ([dbcbbcb](https://github.com/Kehl-io/nestweaver/commit/dbcbbcba3f862d47e0fb873c158c4e974fe98290))
* **cli:** make search limit honest and enforce float/enum flag contracts ([#206](https://github.com/Kehl-io/nestweaver/issues/206)) ([3981f48](https://github.com/Kehl-io/nestweaver/commit/3981f48a4bd134da0ee98b3e0a5602de01865564))
* **cli:** one JSON envelope for impact, and dead-code parity across paths ([#218](https://github.com/Kehl-io/nestweaver/issues/218)) ([85b8146](https://github.com/Kehl-io/nestweaver/commit/85b81463e00a74f095cebddaa42ada65c9e80846))
* **cli:** report db_not_found from every read-only open, not just some ([#209](https://github.com/Kehl-io/nestweaver/issues/209)) ([81039bb](https://github.com/Kehl-io/nestweaver/commit/81039bb87251a674239b7b6ec5511491d1cce620))
* **cli:** resolve brain refresh against the existing vault registration ([#212](https://github.com/Kehl-io/nestweaver/issues/212)) ([e21a2f7](https://github.com/Kehl-io/nestweaver/commit/e21a2f7b8b5c040cc64f3e2198173ad976b8ecbe))
* **daemon:** log boot timing so a stalled start is diagnosable ([#221](https://github.com/Kehl-io/nestweaver/issues/221)) ([1b2d32c](https://github.com/Kehl-io/nestweaver/commit/1b2d32c383c91372245b7e756d7fa2ee7655f753))
* **engine:** disclose when read_symbols exceeds the requested token budget ([#217](https://github.com/Kehl-io/nestweaver/issues/217)) ([9871af2](https://github.com/Kehl-io/nestweaver/commit/9871af233cee97a31fff32a005d31b5801b7395b))
* **engine:** disclose when the co-change sidecar does not cover a changed file ([#201](https://github.com/Kehl-io/nestweaver/issues/201)) ([82723b8](https://github.com/Kehl-io/nestweaver/commit/82723b858caf9000cc90b9773a1215c92108d495))
* **engine:** link declared gRPC contracts to their Rust/tonic implementations ([#215](https://github.com/Kehl-io/nestweaver/issues/215)) ([cee6a2e](https://github.com/Kehl-io/nestweaver/commit/cee6a2ecfff0de21c8d351b43a23011c87c85ca7))
* **engine:** repo-qualify the co-change sidecar instead of overwriting it ([#220](https://github.com/Kehl-io/nestweaver/issues/220)) ([4133a16](https://github.com/Kehl-io/nestweaver/commit/4133a167e9401ec316955ac85f8a6a39e92f1e69))
* **engine:** resolve path-qualified wikilinks, and stop indexing external urls ([#213](https://github.com/Kehl-io/nestweaver/issues/213)) ([3904efb](https://github.com/Kehl-io/nestweaver/commit/3904efb79561da41182e9cd95502b2837b159e58))
* **engine:** stop reporting resolved wikilinks as broken ([#205](https://github.com/Kehl-io/nestweaver/issues/205)) ([84ae503](https://github.com/Kehl-io/nestweaver/commit/84ae5030d743f51069e6b6bff971c15a0f36fc32))
* **engine:** suggest broken-link targets by filename stem, and stop over-promising ([#214](https://github.com/Kehl-io/nestweaver/issues/214)) ([987064c](https://github.com/Kehl-io/nestweaver/commit/987064cb01c5bbd2aa0380e4a7feb819965fcd53))
* **parser:** accept non-ASCII inline tag names ([#207](https://github.com/Kehl-io/nestweaver/issues/207)) ([a389fe1](https://github.com/Kehl-io/nestweaver/commit/a389fe10d814726ac1080612933d313eee8d72b1))
* **parser:** stop double-encoding non-ASCII in wikilinks and tags ([#204](https://github.com/Kehl-io/nestweaver/issues/204)) ([1d257dc](https://github.com/Kehl-io/nestweaver/commit/1d257dc0c7bc03908ce783b7d6a1b89105e5cafc))
* **store:** conserve the wikilink graph across an instance merge ([#211](https://github.com/Kehl-io/nestweaver/issues/211)) ([53ff548](https://github.com/Kehl-io/nestweaver/commit/53ff54872eb809ecdb5def69afe44368becb8054))
* **store:** label the edge type flow_trace followed to each callee ([#216](https://github.com/Kehl-io/nestweaver/issues/216)) ([2ba732d](https://github.com/Kehl-io/nestweaver/commit/2ba732df05c85b3e8962985d8842d97a52df2649))
* **daemon (data safety):** a SIGKILLed daemon left an orphaned WAL that made the database unopenable by *every* path, and the resulting error misclassified a live 5.6 GB database as missing — advising `index --repo`, which would have written a new database over recoverable data. The orphaned WAL is now quarantined by rename (never deleted, since a log can hold committed work) and diagnosis consults the filesystem instead of the error text; nw-126 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **daemon:** a daemon boot timeout silently fell back to the direct path — exit 0, nothing on stderr, an authoritative-looking result — while requesting that same bypass explicitly is refused with a WAL-corruption warning. Both routes now disclose the bypass, naming the RPC and the cause; nw-125 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **store:** `hubs` and `bridges` now report on stderr when a database holds repos indexed by an older resolver, so a stale ranking is visible rather than silently served — see Upgrade Notes; nw-124 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **index:** a no-op incremental index (zero changed files) walked every call site against the full graph, costing 3440.6s and exiting 1 with "index progress stream ended before completion" for work that had in fact succeeded. Measured on the same repo and command: **3440.6s exit 1 → 18s exit 0**. A timeout now reports a terminal error in-band naming the variable to raise, instead of truncating the stream; nw-127 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **daemon:** boot spent 77% of its time on a full-graph extension liveness walk; **53.9s → 9.5s**. Boot phase timings are now logged at bind so a slow start is diagnosable; nw-119 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **daemon:** boot-failure messages pointed at the stderr log, which is guaranteed not to contain the tracing error being hunted — the dated rolling file holds it. All ten operator-facing pointers now name the directory and distinguish the two files; nw-118 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **investigate:** the CLI's direct path is BM25-only while the daemon applies semantic ranking, so the two returned materially different orderings with nothing to explain why. `InvestigateResult` now carries `semantic_applied` and `degraded_components`, and the text renderer states that ranking is lexical-only; nw-120 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **brain:** `broken-links` printed a piped wikilink's *display alias* as though it were the link, so grepping the vault for the reported text found nothing. Wikilink edges now carry the target alongside the display text; backlinks still show the alias, which is correct there; nw-122 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **brain context:** semantic nearest neighbours were counted as resolved seeds, so a nonsense query exited 0 and printed a confident-looking result; nw-102 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **cli:** `brain status` on the direct path omitted the entire Embedding block, and the `impact` JSON envelope's key set varied by truncation path; nw-121, nw-123 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))
* **cli,mcp,store:** `regex-search --json` omitted the honesty note MCP already returned; `query_extensions` key+value matching returned 0 results against array-valued properties (scalar-in-array membership now matches); `clusters --json` recomputed on every call while `--help` promised a cache; nw-097, nw-109, nw-075 ([#222](https://github.com/Kehl-io/nestweaver/issues/222)) ([4bade71](https://github.com/Kehl-io/nestweaver/commit/4bade712ce3f327045efb1fed6ad8fb3aa13a928))

## [2.7.1](https://github.com/Kehl-io/nestweaver/compare/v2.7.0...v2.7.1) (2026-07-29)


### Bug Fixes

* **blast-radius:** a truncated traversal must not report status Complete ([#198](https://github.com/Kehl-io/nestweaver/issues/198)) ([687b4a5](https://github.com/Kehl-io/nestweaver/commit/687b4a58ed61a6b4a8e76e4d25f80a556ede0bca))
* **cli:** surface honesty fields the text renderers were dropping, and enforce parity ([#196](https://github.com/Kehl-io/nestweaver/issues/196)) ([83349ea](https://github.com/Kehl-io/nestweaver/commit/83349eae6a537cac9080a55b0741869fb8f5b632))
* import edge fan-out corrupting hub ranking, and impact presenting a capped set as complete ([#195](https://github.com/Kehl-io/nestweaver/issues/195)) ([3f06f08](https://github.com/Kehl-io/nestweaver/commit/3f06f0885fb30b558020150507645ac9d9f7cb60))
* **list-projects:** fail on a missing database instead of reporting zero projects ([#197](https://github.com/Kehl-io/nestweaver/issues/197)) ([a6fa945](https://github.com/Kehl-io/nestweaver/commit/a6fa9452f9fd1c2feed5ce503990c57811e5d2c1))

## [2.7.0](https://github.com/Kehl-io/nestweaver/compare/v2.6.3...v2.7.0) (2026-07-28)


### Features

* add instance config validation command ([ef1bb40](https://github.com/Kehl-io/nestweaver/commit/ef1bb40f5e856996ae3a30213ce90b1e0689b854))
* **daemon:** add embedding preflight ([#189](https://github.com/Kehl-io/nestweaver/issues/189)) ([635d13d](https://github.com/Kehl-io/nestweaver/commit/635d13da5f794616872c52b2dcdb3e322caa6585))
* make embedding device selection explicit ([7c99573](https://github.com/Kehl-io/nestweaver/commit/7c99573146eaecce1ee79c6e8d8f26d955924a2e))
* report embedding device and readiness ([48d7a11](https://github.com/Kehl-io/nestweaver/commit/48d7a1103a537dfb94a6d4b779e399e43a632b71))


### Bug Fixes

* avoid fork daemonization on macOS ([1658e3f](https://github.com/Kehl-io/nestweaver/commit/1658e3f9e3ed8e4635edc4640d82c61990a8f5ca))
* **daemon:** honor embedding sidecar in incremental runs ([92dafe4](https://github.com/Kehl-io/nestweaver/commit/92dafe4371d2070d5e9a9306f571e189acfd6a03))
* **daemon:** make incremental embedding idempotent ([#188](https://github.com/Kehl-io/nestweaver/issues/188)) ([eaaa43c](https://github.com/Kehl-io/nestweaver/commit/eaaa43cb494d22aa563980505dafa362b582cdbb))
* **deps:** pin Ladybug filtered scan correction ([a0afec2](https://github.com/Kehl-io/nestweaver/commit/a0afec260b613e0d63f1b954ee66657faf6c1bc6))
* **embed:** honor cache remediation and fail fast ([ca9ad5d](https://github.com/Kehl-io/nestweaver/commit/ca9ad5dafd34752d910fa00228b45d5bcae4d5c8))
* **embed:** quote cache remediation arguments ([5640210](https://github.com/Kehl-io/nestweaver/commit/56402106d7fb67c5473039e4dd03b29c35696ac5))
* **embed:** resolve artifacts through configured cache ([5bc0ba3](https://github.com/Kehl-io/nestweaver/commit/5bc0ba32ca1d77093062a8f182be9ef227447466))
* harden embedding readiness publication ([8474cf1](https://github.com/Kehl-io/nestweaver/commit/8474cf1149ff27bc640083af6a7c14edc8413818))
* harden macOS daemon startup ([dee89de](https://github.com/Kehl-io/nestweaver/commit/dee89de0690ac0ea96457884765b6a0c13518ff7))
* honor follower cancellation in single flight ([9b5c0df](https://github.com/Kehl-io/nestweaver/commit/9b5c0dfe62a8cf1a05a96734aee7325b573f0521))
* preserve embedding compatibility ([e94dbe9](https://github.com/Kehl-io/nestweaver/commit/e94dbe9ab6de2b805aaae5ef9c8c6276eb067723))
* preserve local embed API ([0ab8a27](https://github.com/Kehl-io/nestweaver/commit/0ab8a27fdacc1ce4513ce57a6046e252a33eaa39))
* satisfy embedding clippy lints ([7973065](https://github.com/Kehl-io/nestweaver/commit/79730651c22b0c23a45ebbad23c87b44ec76df28))
* select CPU for auto without Metal ([f061283](https://github.com/Kehl-io/nestweaver/commit/f0612830cdb976f6799ddc1cdf156f5bb61b339a))
* **store:** cascade orphaned note fragments ([934b108](https://github.com/Kehl-io/nestweaver/commit/934b108cc90870e636d308b0279f4f0b6361d2ae))


### Performance Improvements

* **store:** add impact snapshot equivalence foundation ([7d759d8](https://github.com/Kehl-io/nestweaver/commit/7d759d87317e4910c96869a026c0898df3f30064))
* **store:** bulk-load impact snapshot endpoints ([4625a34](https://github.com/Kehl-io/nestweaver/commit/4625a34bcdaeb3d59130cbe9b1038ed9a15dff22))
* **store:** cache impact snapshots by generation ([b21abf6](https://github.com/Kehl-io/nestweaver/commit/b21abf6c854d64056e3f6137d0e2adbe95ae86cc))

## [2.6.3](https://github.com/Kehl-io/nestweaver/compare/v2.6.2...v2.6.3) (2026-07-25)


### Bug Fixes

* **ci:** gate releases on lockfile synchronization ([b3b863c](https://github.com/Kehl-io/nestweaver/commit/b3b863c0bce34cf689138b47d6a1bb4748965949))
* **ci:** gate releases on lockfile synchronization ([6b5cda6](https://github.com/Kehl-io/nestweaver/commit/6b5cda6f24546c1c1ba755545445aa656a283a06))
* **ci:** keep test artifacts within runner disk ([1f75f5c](https://github.com/Kehl-io/nestweaver/commit/1f75f5c6569c9cc80ad8bf109dd5d5566c3bc240))

## [2.6.2](https://github.com/Kehl-io/nestweaver/compare/v2.6.1...v2.6.2) (2026-07-25)


### Bug Fixes

* **ci:** pin tree-sitter-kotlin to an exact rev ([1e46e97](https://github.com/Kehl-io/nestweaver/commit/1e46e978a2aa5934f0ee101104b16184697e1b0b))
* **ci:** repair release builds (pin tree-sitter-kotlin) and speed up CI ([cc075cd](https://github.com/Kehl-io/nestweaver/commit/cc075cded83f2081b73d60bd8280ba10818661a6))

## [2.6.1](https://github.com/Kehl-io/nestweaver/compare/v2.6.0...v2.6.1) (2026-07-25)


### Bug Fixes

* **daemon:** bake NESTWEAVER_INDEX_CPU_PERCENT into the launchd plist ([906b1ae](https://github.com/Kehl-io/nestweaver/commit/906b1ae186fe9923bec699be71a997e1337d203c))
* **daemon:** reload launchd plist on install; add LowPriorityIO and ThrottleInterval ([c711a05](https://github.com/Kehl-io/nestweaver/commit/c711a050a4056b076cab35ad32903780c10de55d))
* **engine:** self-heal incomplete index on the server worker path ([8eb8854](https://github.com/Kehl-io/nestweaver/commit/8eb8854a95b9c68451720e90bd8f297a5580a9ab))
* **engine:** survive and self-heal mid-index process kills ([764f587](https://github.com/Kehl-io/nestweaver/commit/764f58759c7b135cf7160973f6d7c2d96d439b59))
* **engine:** survive and self-heal mid-index process kills ([5ae091a](https://github.com/Kehl-io/nestweaver/commit/5ae091a1ea68d8c2bfec01bf0303b3f7a4172683))
* **mcp:** drop anyOf from tool schemas so strict providers accept them ([9205501](https://github.com/Kehl-io/nestweaver/commit/9205501ac91aebce809d602626575f9ee7ac7abc))
* **mcp:** drop anyOf from tool schemas, keep root "type": "object" ([c2e7810](https://github.com/Kehl-io/nestweaver/commit/c2e7810c6d8bc5631972ba20402427eba092feb0))
* **mcp:** move "type": "object" into anyOf items in tool schemas ([d2bdba3](https://github.com/Kehl-io/nestweaver/commit/d2bdba36a81a0848e1b726f34b38621e3bab0cc3))
* share the incomplete-index probe across stale-check paths ([ca2e54b](https://github.com/Kehl-io/nestweaver/commit/ca2e54b846ce56b770ee16d0d1c1c5211c2934cb))
* **store:** probe repo content across File and vault Note nodes ([f855313](https://github.com/Kehl-io/nestweaver/commit/f8553134183a71a86c138927d60deafa0befe39b))

## [2.6.0](https://github.com/Kehl-io/nestweaver/compare/v2.5.11...v2.6.0) (2026-07-24)


### Features

* **cli:** rts-eval record-truth and report subcommands + CI docs ([1615073](https://github.com/Kehl-io/nestweaver/commit/16150738e2270c4c74d69ee70199f7a8d3196041))
* **engine,mcp:** surface git co-change coupling in blast radius ([d8b5747](https://github.com/Kehl-io/nestweaver/commit/d8b57477c09ec6093a826c242e3672d0715bcf1d))
* **engine:** machine-readable run-full-suite recommendation on affected_tests ([d1c2f40](https://github.com/Kehl-io/nestweaver/commit/d1c2f402bfe7725f5a31ab96ceed014d3d9eadd6))
* **engine:** rts_eval measured-recall loop — recording, metrics, in-band disclosure ([7d8d96e](https://github.com/Kehl-io/nestweaver/commit/7d8d96e6df68b97c1b0867020e9c095dd8c3975f))
* **engine:** TIA-style always-include rules and stale-index disclosure for affected_tests ([7d78531](https://github.com/Kehl-io/nestweaver/commit/7d78531dbe521f9628a00fb31347d15f48e71bc7))
* **mcp:** compile advertised tool schemas for validation ([3496344](https://github.com/Kehl-io/nestweaver/commit/349634489d1a0c996098a55bbe04ff1372b1033a))
* **search:** expose bounded result cardinality alongside ranked hits ([127702e](https://github.com/Kehl-io/nestweaver/commit/127702e17e46c44aeb4465b90e24061d5d0467e5))


### Bug Fixes

* address PR review — rts-eval integrity, trigram rebuild safety, CI probe env ([f1ec712](https://github.com/Kehl-io/nestweaver/commit/f1ec712e23b0e32edabc792b4a5379ff0755522f))
* **affected_tests:** select Rust inline #[test] functions (nw-085 part B) ([46c9630](https://github.com/Kehl-io/nestweaver/commit/46c963039ba31e815c972420f06ecdb6131e3998))
* **affected-tests:** fail closed on symbol enumeration errors ([989c4d5](https://github.com/Kehl-io/nestweaver/commit/989c4d5d43065245cae49607e2a8654f470e8906))
* **authz:** close blast-radius response leaks ([79239f9](https://github.com/Kehl-io/nestweaver/commit/79239f9bcb3a529932fb039eeeb03473cd3d3942))
* **authz:** fail closed on unknown symbol ownership ([fbe665f](https://github.com/Kehl-io/nestweaver/commit/fbe665fd2a10f2d7a3df3eb0082ae77d1578d79a))
* blast-radius trust & accuracy batch — verdict integrity, live centrality boost, CI fail-safe signal, co-change tier ([8399125](https://github.com/Kehl-io/nestweaver/commit/83991255cf699f593898a32aa8d4269fbca81d04))
* **blast-radius:** report total affected count without authz leaks ([2e986e5](https://github.com/Kehl-io/nestweaver/commit/2e986e54cd950ed60130f80b562fb23879ecbbfc))
* bug-hunt batch — search integrity, MCP validation, daemon lifecycle, CLI correctness ([fbd8d54](https://github.com/Kehl-io/nestweaver/commit/fbd8d54ffa3f568e5d041acf71257c4460af3b70))
* **ci:** gate pid_exited_within_grace to macOS ([09fe2d7](https://github.com/Kehl-io/nestweaver/commit/09fe2d7fd9e9ab143c09653bb45601f044477cb0))
* **cli:** don't double-warn about the daemon bypass on autostart ([6112db2](https://github.com/Kehl-io/nestweaver/commit/6112db228bbf602a5e97ea2b773a27f1354c64e1))
* **cli:** hard-disable the daemon bypass outside CI ([56eed52](https://github.com/Kehl-io/nestweaver/commit/56eed5204c80bb136bbc490309f0feb2366ae27d))
* **cli:** impact --json parity, missing-db guard, and CLI nits (nw-086/087/088) ([66667f0](https://github.com/Kehl-io/nestweaver/commit/66667f0fb18620de0bcebd99a8357fe1d746272d))
* **clustering:** make Leiden community labels deterministic (nw-081) ([ab145df](https://github.com/Kehl-io/nestweaver/commit/ab145dff0030f1660b9107537b344ad2c3a336a9))
* **daemon:** committed admin mutations report success-with-warnings, never a bare error (nw-091 Bug 2) ([249cc17](https://github.com/Kehl-io/nestweaver/commit/249cc1796cb4660c4e82688a7d201717e7fa593a))
* **daemon:** self-heal wedged migration journals + operator abort-migration recovery ([cf22844](https://github.com/Kehl-io/nestweaver/commit/cf22844deeaa57dd5a0d80f49f782ecd95ef314a))
* **engine:** carry repo_uid on ChangedSymbolRef for selection recording ([74af722](https://github.com/Kehl-io/nestweaver/commit/74af722d2065e5ecceb03fc68c4ff7075d7fb343))
* **engine:** compute blast-radius risk and gate from the pre-truncation set ([aab53e2](https://github.com/Kehl-io/nestweaver/commit/aab53e2620d4b37dc30725c4c75ba2ca4730470b))
* **engine:** fail safe on unindexed changed source files ([70a6a78](https://github.com/Kehl-io/nestweaver/commit/70a6a78e5b86773c4dd53cbeb4a19817cf393e52))
* **engine:** hydrate changed-symbol PageRank from the ranking cache ([324ed79](https://github.com/Kehl-io/nestweaver/commit/324ed7958cba5a011b8d364c8fa51a924ecc683d))
* **engine:** make measured recall flakiness-aware, and document the honest prior ([0e479c2](https://github.com/Kehl-io/nestweaver/commit/0e479c2bac027ad9d96637f96479acc12f4577bf))
* **engine:** preserve handoffs on migration abort ([674ce7b](https://github.com/Kehl-io/nestweaver/commit/674ce7b031af5191b1d4bd6453373f4b4c026761))
* full-diff review round 5 — resolver spans, mutation wrappers, daemon/cli honesty ([94212b5](https://github.com/Kehl-io/nestweaver/commit/94212b5f39ca3690d316fcf570802a805c0b6dd4))
* hunt-round-2 accuracy — watcher edge preservation, cross-crate resolution, impact honesty, perf ([c265750](https://github.com/Kehl-io/nestweaver/commit/c265750a5289a9a9dc4e76f9fb22c2d674e047c4))
* **investigate:** union per-token seeds for multi-word queries (nw-080) ([a4fa100](https://github.com/Kehl-io/nestweaver/commit/a4fa1006f3c7ecd02f1234defd6bb03d645087a6))
* **mcp:** bound schema validation errors ([262d810](https://github.com/Kehl-io/nestweaver/commit/262d8108eeaeb9a08d96321e245e3d050c27b2b5))
* **mcp:** constrain scoped impact traversal ([39930e4](https://github.com/Kehl-io/nestweaver/commit/39930e4d0021dd6e6244dd152990cfc4999af6f6))
* **mcp:** honesty + validation nits across read_symbols/get_summary/investigate (nw-084) ([fff24f5](https://github.com/Kehl-io/nestweaver/commit/fff24f5c91f555e8904037362ba2ba83e32c3262))
* **mcp:** make brain search totals independent of display limits ([984c474](https://github.com/Kehl-io/nestweaver/commit/984c474031f74ecfa27294c3d3ebaa0773c684ba))
* **mcp:** repair the daemon-proxy write tools — remove/add source, extension cache (nw-089) ([a330d6e](https://github.com/Kehl-io/nestweaver/commit/a330d6eab35e363f134c3fbbcbf1cd88f4dcf2f2))
* **mcp:** scope local two-tier results ([46ee7d0](https://github.com/Kehl-io/nestweaver/commit/46ee7d01b8f7d9383951be9fc0d9e0166c7efe85))
* **mcp:** stop caching read_symbols (nw-077) ([192b222](https://github.com/Kehl-io/nestweaver/commit/192b22232b4d05276d726162d874c1d6114aeaf0))
* **mcp:** validate arguments before every transport dispatch ([64abd1a](https://github.com/Kehl-io/nestweaver/commit/64abd1ad36ccbe89b947d79b247f0780f6b03372))
* **mcp:** validate HTTP arguments before routing ([1084c9d](https://github.com/Kehl-io/nestweaver/commit/1084c9dceb01b096753a4a8980eaf764c4505da0))
* **mcp:** withhold org impact for restricted callers ([c7a6765](https://github.com/Kehl-io/nestweaver/commit/c7a6765121b26b82d3f998fc23beba13b003a8ab))
* **parser:** mark exported TS/JS declarations Public via export_statement ancestors ([934140a](https://github.com/Kehl-io/nestweaver/commit/934140ac2fb8d189a108683deea510be778516d9))
* **process:** bound detect_changes with an in-memory scoped trace (nw-078) ([bf1b163](https://github.com/Kehl-io/nestweaver/commit/bf1b16391d224e12cb98f16a8a9204d029b5a709))
* regex_search drops real matches; affected_tests base_ref; clusters paging (nw-076/088/090) ([1e4673d](https://github.com/Kehl-io/nestweaver/commit/1e4673df7eb4e4ae0a113557c6bcae16c486b628))
* **resolver:** never resolve Rust builtin crates to local modules ([d00d197](https://github.com/Kehl-io/nestweaver/commit/d00d19740de6f37e5c86d541c7669dd5fd43f68b))
* review round 2 + manual-test findings ([aeefa19](https://github.com/Kehl-io/nestweaver/commit/aeefa19f24fc85cff4ac5473c66b03cbe5465384))
* review round 3 — rts-eval cache/join/lock correctness, repo-name default ([a3b9eaf](https://github.com/Kehl-io/nestweaver/commit/a3b9eafd3db582368e7f83f0d08de1dd9f1c6653))
* review round 4 — rts-eval, embeddings, watcher, resolver, alias bindings ([1805da4](https://github.com/Kehl-io/nestweaver/commit/1805da462308f05e1080a0393fe9198d55314cb6))
* **search:** enforce authz and indexed ownership ([34930e7](https://github.com/Kehl-io/nestweaver/commit/34930e7305e47dfc75df7542fa5c5c5065480540))
* **search:** fail closed across hybrid transports ([a721484](https://github.com/Kehl-io/nestweaver/commit/a7214849d039ba18436e8e7bde42fe7c96f785db))
* **search:** harden counted result contracts ([d36f7d3](https://github.com/Kehl-io/nestweaver/commit/d36f7d3c5f7ddac58d6a48d461fc530272f8d1d7))
* **search:** keep legacy counts conservative ([a1a927f](https://github.com/Kehl-io/nestweaver/commit/a1a927f06619020f2ddc2616d48b67f9173d295f))
* **search:** preserve honest totals through daemon and federation ([87bc3dc](https://github.com/Kehl-io/nestweaver/commit/87bc3dcc81828f8856334dc56d6c2d55f33d89de))
* **search:** reject noncanonical symbol lines ([75ee4f6](https://github.com/Kehl-io/nestweaver/commit/75ee4f63e5750cc45f6319109fb279d6fee71737))
* **search:** reject reads from migrated index handles ([3e5196c](https://github.com/Kehl-io/nestweaver/commit/3e5196cebef5dcdc65f9900c3c5177b7eb2a21a0))
* **search:** use stable symbol identity across tiers ([81fb7e8](https://github.com/Kehl-io/nestweaver/commit/81fb7e80d10b88ae50273f1919e063b5d50b431e))
* **search:** validate hybrid identity proofs ([8a8966c](https://github.com/Kehl-io/nestweaver/commit/8a8966ca94471d5b56c29628cc642be2dd4266db))
* **store:** bound the engine thread pool on write opens to close the crash race ([33ae54d](https://github.com/Kehl-io/nestweaver/commit/33ae54d7cc0896645ba0dfc2220821bbc20e91e0))
* **store:** classify destructive mutation outcomes ([433b77a](https://github.com/Kehl-io/nestweaver/commit/433b77a167e2e2747bcc2fdc7873f50d2aedf428))
* **store:** loud NUL-corruption canary at the string-extraction choke point ([3358e48](https://github.com/Kehl-io/nestweaver/commit/3358e486d4aafa4fff0ac633c8f0ec8b549d5648))
* **store:** recover an abandoned index-publication generation reservation (nw-091 Bug 3A) ([481b6fc](https://github.com/Kehl-io/nestweaver/commit/481b6fc32e0ea585dadbd611aa9b55cc8bdb180a))
* **store:** repair impact-traversal display fields via primary-key lookups ([22159d8](https://github.com/Kehl-io/nestweaver/commit/22159d81eaa96caf33057821532f1cf90c7af833))
* **summary:** bound symbol-level summaries and push target down (nw-079) ([897e0fa](https://github.com/Kehl-io/nestweaver/commit/897e0fa4f313961646790787f02eb2f4f44ca4f6))


### Performance Improvements

* **affected_tests:** walk the reverse-impact graph in memory (nw-085 part A) ([6fe4eb4](https://github.com/Kehl-io/nestweaver/commit/6fe4eb424c2a68e86dd14b06a93f3dc0a55e08b7))

## [2.5.11](https://github.com/Kehl-io/nestweaver/compare/v2.5.10...v2.5.11) (2026-07-21)


### Bug Fixes

* user-reported 2.5.10 bugs — affected_tests crash/hang, top-level stale-check, trust-contract casing docs ([#165](https://github.com/Kehl-io/nestweaver/issues/165)) ([eb9d3e4](https://github.com/Kehl-io/nestweaver/commit/eb9d3e4f0782b7b6a6291a100346bf876ed80fb8))

## [2.5.10](https://github.com/Kehl-io/nestweaver/compare/v2.5.9...v2.5.10) (2026-07-20)


### Bug Fixes

* **release:** require macOS 13.3 ([325ccf1](https://github.com/Kehl-io/nestweaver/commit/325ccf11e378bf0115ddf3b1ccaf17f9e4672652))
* **release:** require macOS 13.3 ([790857a](https://github.com/Kehl-io/nestweaver/commit/790857a2213ea797b731487fd5e8fd41972056c4))

## [2.5.9](https://github.com/Kehl-io/nestweaver/compare/v2.5.8...v2.5.9) (2026-07-20)


### Bug Fixes

* **release:** use portable lipo syntax ([2256a9b](https://github.com/Kehl-io/nestweaver/commit/2256a9b9af41799a603e83012945a5c34095a83f))
* **release:** use portable lipo syntax ([33c4620](https://github.com/Kehl-io/nestweaver/commit/33c462095c594eb230970bba89c8daf261c657a2))

## [2.5.8](https://github.com/Kehl-io/nestweaver/compare/v2.5.7...v2.5.8) (2026-07-20)


### Bug Fixes

* **release:** link Intel macOS compiler runtime ([ba06971](https://github.com/Kehl-io/nestweaver/commit/ba069717dde867a7b79688d115a56db9920832dd))
* **release:** link Intel macOS compiler runtime ([d6e2bb2](https://github.com/Kehl-io/nestweaver/commit/d6e2bb24b6fe17798aee4176a6673730b9607917))

## [2.5.7](https://github.com/Kehl-io/nestweaver/compare/v2.5.6...v2.5.7) (2026-07-20)


### Bug Fixes

* **release:** build Intel macOS natively ([#157](https://github.com/Kehl-io/nestweaver/issues/157)) ([a7569d5](https://github.com/Kehl-io/nestweaver/commit/a7569d508ecc22ef04e31bfb2cd748aed4252f7a))

## [2.5.6](https://github.com/Kehl-io/nestweaver/compare/v2.5.5...v2.5.6) (2026-07-20)


### Bug Fixes

* **release:** build Linux ARM64 natively ([#155](https://github.com/Kehl-io/nestweaver/issues/155)) ([22df618](https://github.com/Kehl-io/nestweaver/commit/22df618e30d7aa9aaa5d66aa7d7cc21f15530bf6))

## [2.5.5](https://github.com/Kehl-io/nestweaver/compare/v2.5.4...v2.5.5) (2026-07-20)


### Bug Fixes

* **release:** avoid stale native build cache ([#153](https://github.com/Kehl-io/nestweaver/issues/153)) ([7bea500](https://github.com/Kehl-io/nestweaver/commit/7bea500a5817b86e3d27d4b3775332137e119799))

## [2.5.4](https://github.com/Kehl-io/nestweaver/compare/v2.5.3...v2.5.4) (2026-07-20)


### Bug Fixes

* **release:** pin ARM64 OpenSSL in CMake toolchain ([#151](https://github.com/Kehl-io/nestweaver/issues/151)) ([52e35c9](https://github.com/Kehl-io/nestweaver/commit/52e35c9af4c826485e7a2090cb019a79eaf4b239))

## [2.5.3](https://github.com/Kehl-io/nestweaver/compare/v2.5.2...v2.5.3) (2026-07-20)


### Bug Fixes

* **release:** isolate ARM64 OpenSSL discovery ([#149](https://github.com/Kehl-io/nestweaver/issues/149)) ([d0f4535](https://github.com/Kehl-io/nestweaver/commit/d0f4535bd23ce646e85a889066dd8c739751dcee))

## [2.5.2](https://github.com/Kehl-io/nestweaver/compare/v2.5.1...v2.5.2) (2026-07-20)


### Bug Fixes

* **release:** guard missing release PR output ([93646d5](https://github.com/Kehl-io/nestweaver/commit/93646d5361b3cd49f868a38c052999fca75a9d30))
* **release:** guard missing release PR output ([acfba65](https://github.com/Kehl-io/nestweaver/commit/acfba65201da0fb4fe19cb9e0a6107726a769b9c))

## [2.5.1](https://github.com/Kehl-io/nestweaver/compare/v2.5.0...v2.5.1) (2026-07-19)


### Bug Fixes

* **release:** synchronize Cargo lockfile in release PRs ([17619a0](https://github.com/Kehl-io/nestweaver/commit/17619a0b09708bec4d30631e08b999ed3fbb2458))
* **release:** synchronize Cargo lockfile in release PRs ([ed5c404](https://github.com/Kehl-io/nestweaver/commit/ed5c404ad295c9b1bce1b20d13722fd5ec0100e3))

## [2.5.0](https://github.com/Kehl-io/nestweaver/compare/v2.4.0...v2.5.0) (2026-07-19)


### Features

* **cli:** add nestweaver index --setup explicit opt-in (nw-023) ([243e3fa](https://github.com/Kehl-io/nestweaver/commit/243e3fa880475dec83f3e523a0ed4711c5775f98))
* **cli:** run gated auto-setup on the default daemon index path (nw-023) ([082d1a6](https://github.com/Kehl-io/nestweaver/commit/082d1a60277ce723de2e89843faf6ca1f338a3cc))
* **daemon,cli:** thread index --instance through the RPC; backup manifest uses logical instance (nw-019) ([0e875ce](https://github.com/Kehl-io/nestweaver/commit/0e875cee77b6025a02b955a4b97502ed1c8ba556))
* **engine:** --instance mismatch error explains the merge migration (nw-019) ([6578109](https://github.com/Kehl-io/nestweaver/commit/65781097bb3bbf1b310f073bdcae0b4db5946fd6))
* **setup:** add pure should_auto_setup gate predicate (nw-023) ([42af32b](https://github.com/Kehl-io/nestweaver/commit/42af32bbeaf3afb6f01d225ba440589579d4e69e))
* **store,cli:** instance merge reports repos that need re-indexing (nw-019) ([9bb8cbc](https://github.com/Kehl-io/nestweaver/commit/9bb8cbc814fa26a11d2bd6160af04bb0cef512b9))
* **web-ui:** impact request timeout+abort, honest loading state, debounced rank-ready refresh (nw-029) ([0aebde8](https://github.com/Kehl-io/nestweaver/commit/0aebde8230bcad6c0e8d759875689b07399c2963))
* **web,daemon:** pre-warm pagerank when serving the UI (nw-029) ([835f0cb](https://github.com/Kehl-io/nestweaver/commit/835f0cb7d95a3fbc9ca0479519f319d9c027925a))


### Bug Fixes

* **authz:** fail loud on repo-listing store errors, retry once; keep fail-closed for empty stores (nw-043) ([a1e9650](https://github.com/Kehl-io/nestweaver/commit/a1e96506975268ee049eaddf29b43a9a246e8d87))
* **authz:** UDS trusted-admin sees all repos under an enabled policy (nw-050) ([e7c52ad](https://github.com/Kehl-io/nestweaver/commit/e7c52adb3ae338ad4ae6e0a88b776d03fa3109ac))
* **backup:** bind staging to opened store ([0b1106b](https://github.com/Kehl-io/nestweaver/commit/0b1106be7e60ccb2c1d4549df06e38b560d59ee8))
* **build:** make lbug source resolution reproducible ([0d94aa3](https://github.com/Kehl-io/nestweaver/commit/0d94aa3ce7bfc3d78181f28d458286d777cae262))
* **build:** select the resolved lbug source version ([bd4376e](https://github.com/Kehl-io/nestweaver/commit/bd4376ea9fddf8858eadf9f9a26f8cf483aa3e06))
* **cli,daemon,engine:** honor config instance_id on ui-watch + no-daemon index; reject colons in instance_id (nw-046, nw-047, nw-052) ([176d77f](https://github.com/Kehl-io/nestweaver/commit/176d77fccc3eb8c0f91191eb1f85621c2fe4d5fc))
* **cli,daemon:** validate index --instance on the daemon path; fail (no setup) on a daemon index error (P2a, P2b) ([6e55169](https://github.com/Kehl-io/nestweaver/commit/6e55169387713fdd6bcdf80d427fbd437c1b7c10))
* **cli:** brain add/refresh honor config instance_id like brain watch (nw-019) ([bb48147](https://github.com/Kehl-io/nestweaver/commit/bb48147d17faeb99d2c2642074d045ad589009f5))
* **cli:** complete automatic setup retries ([5ae82dc](https://github.com/Kehl-io/nestweaver/commit/5ae82dc1deeb29e61fe1b7a2abf79913a95a6ad0))
* **cli:** gate index auto-setup to TTY + in-repo cwd, anchor writes to repo root (nw-023) ([cf3e269](https://github.com/Kehl-io/nestweaver/commit/cf3e269ffd587c0d5ba8545f9871149a311b95f4))
* **cli:** index status line reports the pagerank mechanism, not a per-run readiness guarantee (nw-029) ([466f64c](https://github.com/Kehl-io/nestweaver/commit/466f64c2fc2de0e1975a1f26463d81175ae4e9f1))
* **cli:** retry incomplete automatic setup ([04d256e](https://github.com/Kehl-io/nestweaver/commit/04d256e7cfa35844ca2f77714a27a603e2ca850c))
* **cli:** top-level watch honors config instance_id like brain watch (nw-019) ([09a9113](https://github.com/Kehl-io/nestweaver/commit/09a9113744bcd62c4ff3d88830181782da2a9c88))
* **cli:** validate --instance flag; snapshot build defaults to config instance not db-path hash (nw-052b, nw-053) ([4c16199](https://github.com/Kehl-io/nestweaver/commit/4c161992e7ddc4208d840b56809edbfda6768633))
* **cli:** validate snapshot build --instance so a colon can't reach the stamp label (nw-052b) ([05e7b93](https://github.com/Kehl-io/nestweaver/commit/05e7b938c7c5a4b3d4ddeda13189fbe095e7ef7b))
* close recovered publication and backup races ([987d23c](https://github.com/Kehl-io/nestweaver/commit/987d23c7269f8a2da6d232d41dc04de48a13f712))
* **daemon,engine,store:** prune-stale drops sidecar slices; invalidate PageRank on code deletions (P1a, P1b) ([e885e4d](https://github.com/Kehl-io/nestweaver/commit/e885e4d5cca4643d2c5f21c921a5f567295b2a4a))
* **daemon:** bump generation on remove-vault/remove-project/prune-stale so post-mutation queries aren't stale (nw-054) ([a49aa77](https://github.com/Kehl-io/nestweaver/commit/a49aa77a23a4ffd3958040b7a51b28b5248f90fb))
* **daemon:** finalize merge and purge cache state ([ec9a9d5](https://github.com/Kehl-io/nestweaver/commit/ec9a9d5b72d9bf3a602479e97b3ce00046e36ae3))
* **daemon:** finalize partial instance mutations ([aceb585](https://github.com/Kehl-io/nestweaver/commit/aceb5858318fa6f5d7c7c23725e873313ad1cfce))
* **daemon:** finalize partial repo deletions ([5af1f7b](https://github.com/Kehl-io/nestweaver/commit/5af1f7bf87f0295a0a3ea2e1e5d286e1afb55784))
* **daemon:** graph writes use the config's logical instance_id, hash stays runtime-only (nw-019) ([7df4477](https://github.com/Kehl-io/nestweaver/commit/7df44779e7be4c7806a3c97912b440625412c270))
* **daemon:** invalidate caches on remove-repo so post-mutation queries don't return deleted data (nw-054) ([8afd4c0](https://github.com/Kehl-io/nestweaver/commit/8afd4c0af5c37aee189c1b7cb4add33dac8b1820))
* **daemon:** reconcile failed instance mutations ([acbef67](https://github.com/Kehl-io/nestweaver/commit/acbef6762b55e4580005836b66d7bfe658886817))
* **daemon:** reconcile registered repo ownership ([f2cd1a7](https://github.com/Kehl-io/nestweaver/commit/f2cd1a75f8ef500374eec7a90fa9ffed2d008844))
* **daemon:** report generation exhaustion ([0cb2517](https://github.com/Kehl-io/nestweaver/commit/0cb25176ada43be6a9bbc171a1885588f3cc7ec7))
* **daemon:** validate all effective instance ids ([baf744a](https://github.com/Kehl-io/nestweaver/commit/baf744adbabdcf29b824625dc01e343cfa6e33f4))
* **daemon:** wait for listener readiness ([e1ceef7](https://github.com/Kehl-io/nestweaver/commit/e1ceef7f7ce402cbf32a47366201ca3ac91235b7))
* **deletions:** propagate reconciliation failures ([7a94fa6](https://github.com/Kehl-io/nestweaver/commit/7a94fa66f952accd0214d5a090b414b8b807a723))
* **deletions:** repair unknown search projections ([d993bea](https://github.com/Kehl-io/nestweaver/commit/d993beaef0460c51a174ec861d58d237763f06e5))
* **deletions:** scope search reconciliation ([4f1c86c](https://github.com/Kehl-io/nestweaver/commit/4f1c86c108c0400ecedfed27c920dbd6136bef6f))
* **deletions:** skip no-op vault reconciliation ([43161c0](https://github.com/Kehl-io/nestweaver/commit/43161c0a80141035f93d25677e3b8a72198749ad))
* durably retire legacy embedding sidecar ([fe416d0](https://github.com/Kehl-io/nestweaver/commit/fe416d00fffba4c73ecb31086470e91a8bad04f6))
* **engine,cli:** compute pagerank at full-index time; load sidecar from canonical path (nw-029) ([c530190](https://github.com/Kehl-io/nestweaver/commit/c53019085e540cdf1e1ca2190e50561cfba44ad7))
* **engine,cli:** compute pagerank on first-index fallback path; correct stale rank status message (nw-029) ([10c2160](https://github.com/Kehl-io/nestweaver/commit/10c2160f7fce161f37b3b55a6c76d94df4963bc5))
* **engine,daemon:** remove-repo drops sidecar slices so re-add re-indexes (nw-048, nw-045) ([8089db5](https://github.com/Kehl-io/nestweaver/commit/8089db5a88780b6389a57821af30827b628d132c))
* **engine:** close deletion finalization gaps ([cfeb9ac](https://github.com/Kehl-io/nestweaver/commit/cfeb9acb3bba7a6e58f480d1f1c7f1e373b4f9ec))
* **engine:** fail closed during index publication ([b63f24e](https://github.com/Kehl-io/nestweaver/commit/b63f24eba72e47ff92df0e428bc5b538ca3c16c6))
* **engine:** fail closed on watcher publication ([6a84cac](https://github.com/Kehl-io/nestweaver/commit/6a84cac8e9aea7183b670fcce7d273dc39023e0f))
* **engine:** fallback-path filemeta parity, re-identify slice hygiene, sidecar polish (nw-022) ([1b7b88a](https://github.com/Kehl-io/nestweaver/commit/1b7b88acb98eeb0850077b073c2d92fa8ad716b6))
* **engine:** finalize indexing graph commits ([d0d13fb](https://github.com/Kehl-io/nestweaver/commit/d0d13fb7285e68c7b7bab9565c83ce5371d508f5))
* **engine:** harden deletion-derived state reconciliation ([b69550d](https://github.com/Kehl-io/nestweaver/commit/b69550da3d1b662f94ddf2d7029ec8464cabbe3a))
* **engine:** per-repo filemeta sidecar (v2) with merge-save and union eviction (nw-022) ([b7a0661](https://github.com/Kehl-io/nestweaver/commit/b7a0661efa7f67560581a3bd868d1427783404f5))
* **engine:** per-repo resolution-deps keying to stop cross-repo stale-edge corruption (nw-045) ([1569a03](https://github.com/Kehl-io/nestweaver/commit/1569a03039168c03a68a8ff8ff8d6bfb634ad69f))
* **engine:** reconcile deletion-derived state ([c34bbfa](https://github.com/Kehl-io/nestweaver/commit/c34bbfa49bcf28fcaa49ece869389fe32a6ad900))
* **index:** require terminal done from daemon streams ([d5904c5](https://github.com/Kehl-io/nestweaver/commit/d5904c5a847b8bb0c58a4911330f4de6e912b2e4))
* **manifests:** migrate legacy suggestion sidecars ([625b5e2](https://github.com/Kehl-io/nestweaver/commit/625b5e2503a46dc8a87006322ca0b6aa19d6d64c))
* **mcp:** charge concise render cost in context budgeting (nw-019) ([67cc21a](https://github.com/Kehl-io/nestweaver/commit/67cc21a527430494a5a99c4462278c8d7f77ce23))
* **mcp:** whole_db_scope_digest walks per-repo filemeta, repo-qualified pairs (nw-022) ([4577e45](https://github.com/Kehl-io/nestweaver/commit/4577e458da5255f383ce2724d7e17f4cda2e9ef2))
* **merge:** harden extension migration recovery ([9555909](https://github.com/Kehl-io/nestweaver/commit/955590982d998e936c1efa78c4587a96db0264a3))
* **merge:** migrate extension metadata across instances ([965eb04](https://github.com/Kehl-io/nestweaver/commit/965eb043a11769de3ca06d49e7d66a1c6a68ef3f))
* **merge:** persist finalizer recovery context ([573121c](https://github.com/Kehl-io/nestweaver/commit/573121cd2913c4b31f3d9b3e6427ce222b4ee7ee))
* **projects:** delete nullable-name records ([44c552c](https://github.com/Kehl-io/nestweaver/commit/44c552c281237932f6baf4255a7f611ce7a59c78))
* **projects:** make deletion transactional ([21e7358](https://github.com/Kehl-io/nestweaver/commit/21e7358fb0964b52a07ff0cf3789d48efdf3448e))
* **projects:** reconcile ambiguous deletion state ([07de8c7](https://github.com/Kehl-io/nestweaver/commit/07de8c74b1e3122d7243b16b8a94a3b32ab288e6))
* **setup:** print the setup banner once per run and honor --quiet on auto-setup (nw-051) ([398492f](https://github.com/Kehl-io/nestweaver/commit/398492f6547b9b6b47733ba09873ccf7b4e12a8e))
* **store,engine:** harden PageRank deletion refresh ([1f715ea](https://github.com/Kehl-io/nestweaver/commit/1f715ea857c50ed7ac53b17619c0c4390702119a))
* **store:** clean source graph during instance merge ([6f90375](https://github.com/Kehl-io/nestweaver/commit/6f90375d7f0307c7ed0b57732b72edf24fb9c1cf))
* **store:** fail closed on unreadable index marker ([854b647](https://github.com/Kehl-io/nestweaver/commit/854b64749877890667142c7827b412817729e60d))
* **store:** harden symbol cache publication ([08e1a9a](https://github.com/Kehl-io/nestweaver/commit/08e1a9a901cc8f0330d548161eae472ef6733995))
* **store:** isolate dirty publication caches ([63ff385](https://github.com/Kehl-io/nestweaver/commit/63ff38578ded655524ea681a405750d28c1be7dd))
* **store:** make graph sidecars crash-safe ([a24f17f](https://github.com/Kehl-io/nestweaver/commit/a24f17f181aeb382aeb3f3adf9252f052a285aa7))
* **store:** preserve durable sidecar metadata ([07398ec](https://github.com/Kehl-io/nestweaver/commit/07398ecfd172466b9e365cf98c19e06a4b7cedd7))
* **store:** prevent dirty generation wraparound ([79eb645](https://github.com/Kehl-io/nestweaver/commit/79eb6453f7468caa0bba496fb0b719e587f06630))
* **store:** serialize index publication lifetimes ([9dfefef](https://github.com/Kehl-io/nestweaver/commit/9dfefef14874abafe61ed320a11ab8c313cf4b4a))
* **store:** single-flight the lazy PageRank compute (nw-029) ([4a78fc5](https://github.com/Kehl-io/nestweaver/commit/4a78fc51a120bb7204e6087a2cf8d6fb8226d9c8))
* **web-ui:** background-refresh impact graph on rank updates instead of flashing to loading (nw-029) ([71f0586](https://github.com/Kehl-io/nestweaver/commit/71f05862c382fbef4261183a1e8545b73865bfb1))
* **web:** preserve API errors and context table results ([4cbf8c9](https://github.com/Kehl-io/nestweaver/commit/4cbf8c9fe74f069e4b705a4fddd2750bd986872d))
* **web:** run pagerank-triggering handlers on blocking threads and emit pagerank:recomputed SSE (nw-029) ([bf535b3](https://github.com/Kehl-io/nestweaver/commit/bf535b368501cedc17b4213f98339f4a879eea62))
* **web:** stabilize frontend refresh behavior ([cd163ea](https://github.com/Kehl-io/nestweaver/commit/cd163ea421407e522dc069cf67d9193c974232b0))

## [2.4.0](https://github.com/Kehl-io/nestweaver/compare/v2.3.4...v2.4.0) (2026-07-16)


### Features

* **blast-radius:** production-grade trust core, contract-diff, per-repo authz, SARIF & pre-push hooks ([#138](https://github.com/Kehl-io/nestweaver/issues/138)) ([442b6f6](https://github.com/Kehl-io/nestweaver/commit/442b6f69d7471daa26a9c26586b20eeade841878))

## [2.3.4](https://github.com/Kehl-io/nestweaver/compare/v2.3.3...v2.3.4) (2026-07-13)


### Performance Improvements

* **web:** gaps endpoint — batch query + generation-gated cache (2.1s → 7ms) ([#133](https://github.com/Kehl-io/nestweaver/issues/133)) ([daebcc9](https://github.com/Kehl-io/nestweaver/commit/daebcc9dc3f0ae3c02ed370d0a71116bb54c2685))

## [2.3.3](https://github.com/Kehl-io/nestweaver/compare/v2.3.2...v2.3.3) (2026-07-12)


### Bug Fixes

* **web:** UI bug hunt — mode labels, notes cleanup, workspace scoping, detail panel polish ([#131](https://github.com/Kehl-io/nestweaver/issues/131)) ([e8a85e2](https://github.com/Kehl-io/nestweaver/commit/e8a85e2e280eb7c674b5a64cec3f2acce9bf5909))

## [2.3.2](https://github.com/Kehl-io/nestweaver/compare/v2.3.1...v2.3.2) (2026-07-11)


### Bug Fixes

* **web:** make the workspace catalog fast at scale + fix breadcrumb label ([#128](https://github.com/Kehl-io/nestweaver/issues/128)) ([140f2c2](https://github.com/Kehl-io/nestweaver/commit/140f2c27990ca6957d8e80b9688fe0df9fcef1cf))

## [2.3.1](https://github.com/Kehl-io/nestweaver/compare/v2.3.0...v2.3.1) (2026-07-09)


### Bug Fixes

* **dist:** repair npm install path and stamp app bundle version ([f732e8a](https://github.com/Kehl-io/nestweaver/commit/f732e8a1a4122b68bafb3242bb274b851d5c4ae7))
* **dist:** repair npm install path and stamp app bundle version ([40dadf9](https://github.com/Kehl-io/nestweaver/commit/40dadf9a0cb714ffdf17698be212a2865356a0cf))
* **dist:** sync Cargo.lock workspace versions to 2.3.0 ([7a37392](https://github.com/Kehl-io/nestweaver/commit/7a3739203c58f70e539afbefb1c6359778f066b6))
* **guide:** generate-guide uses the live tool registry, emits raw markdown ([fdbedbd](https://github.com/Kehl-io/nestweaver/commit/fdbedbda7f9094db6da2c550327ea800bf711701))
* **guide:** generate-guide uses the live tool registry, emits raw markdown ([ada5f9e](https://github.com/Kehl-io/nestweaver/commit/ada5f9e7910b572047ce9061ae613cd6545a6260))

## [2.3.0](https://github.com/Kehl-io/nestweaver/compare/v2.2.1...v2.3.0) (2026-07-08)


### Features

* **web:** search-first task-lens UI overhaul with constellation view ([#122](https://github.com/Kehl-io/nestweaver/issues/122)) ([424d879](https://github.com/Kehl-io/nestweaver/commit/424d879858cd7b9c4d96354a5fec62cfafbd1a11))

## [2.2.1](https://github.com/Kehl-io/nestweaver/compare/v2.2.0...v2.2.1) (2026-07-07)


### Bug Fixes

* address code-review findings on embedding, wikilink, and schema fixes ([5d4317c](https://github.com/Kehl-io/nestweaver/commit/5d4317cf21d7e9387ed8a67f3fa0d53d14508e76))
* embedding model-switch, MCP crash recovery, wikilink resolution, and schema consistency ([899fec4](https://github.com/Kehl-io/nestweaver/commit/899fec4536358da0c4f487f98d04aa38935709a1))
* **embed:** honor --force flag in EmbeddingIndex dimension guard ([be4e64d](https://github.com/Kehl-io/nestweaver/commit/be4e64dd2c8894c3149e4500b16d68d8e6c4be80))
* **embed:** reject --model-id when routed through daemon ([cfd05bf](https://github.com/Kehl-io/nestweaver/commit/cfd05bf84473a2a802f4090cb750db73fddb938e))
* harden embedding add API and clusters sidecar write per review ([ed1d3bc](https://github.com/Kehl-io/nestweaver/commit/ed1d3bc4dd4ce458c15c02b2aa046ae77f2fbfe6))
* **mcp:** persist clusters sidecar and improve tool schema consistency ([a11162b](https://github.com/Kehl-io/nestweaver/commit/a11162bc5a62c85db5c1eafaeba9eaf4d7b422ce))
* **vault:** add global filename-stem resolution for wikilinks ([7911f23](https://github.com/Kehl-io/nestweaver/commit/7911f238d36d59cba92257cf7927a3bee3810a5f))

## [2.2.0](https://github.com/Kehl-io/nestweaver/compare/v2.1.1...v2.2.0) (2026-07-06)


### Features

* add --ca-cert to connect command for self-signed TLS ([9777c43](https://github.com/Kehl-io/nestweaver/commit/9777c43260478c3f08d6469e11e70b60d6e260d3))
* add ca_cert to upstream config for self-signed TLS support ([d28aa19](https://github.com/Kehl-io/nestweaver/commit/d28aa196fbdf8c7c4ff4fb6ae0590d4a4a0093c6))
* **backup:** split into stage_backup_from_store (under lock) + package_staged (lock-free) ([6bbb37c](https://github.com/Kehl-io/nestweaver/commit/6bbb37c459387e6d0088fee095559b65ad6886cc))
* **ci:** emit GitLab Code Quality report from impact analysis ([189d1cd](https://github.com/Kehl-io/nestweaver/commit/189d1cd40438c7ed87bda1dfc3c493c167260ae4))
* **ci:** GitLab CI template for impact analysis ([bfc12f7](https://github.com/Kehl-io/nestweaver/commit/bfc12f74aca684156c96b1595fecbaa7915ad4d1))
* **cli:** add 'server backup' alias for top-level backup command ([06dd238](https://github.com/Kehl-io/nestweaver/commit/06dd23871168be0046eb11e0b7578db1d48437f7))
* **cli:** add server status subcommand and connect --device flag ([3a1cb6a](https://github.com/Kehl-io/nestweaver/commit/3a1cb6aa3d20fd2342231a86c2ca9472551d4889))
* **cli:** backup save uses the single daemon Backup RPC ([53b4aa4](https://github.com/Kehl-io/nestweaver/commit/53b4aa4317e2d7bfa1bb29d253c035380588694a))
* **client:** confidence labeling on hybrid results ([2340c9a](https://github.com/Kehl-io/nestweaver/commit/2340c9a430699e6e3201b63e15226494dc6e3964))
* **client:** device-flow onboarding for connect ([7cfb7c1](https://github.com/Kehl-io/nestweaver/commit/7cfb7c17b95e09aacbfefd443a10d88910eab9a9))
* **client:** explicitly map all 40 MCP tools in routing matrix ([2c93ee3](https://github.com/Kehl-io/nestweaver/commit/2c93ee31c095b33ca7d67ff40481ae952def2a0c))
* **client:** fallback routing mode for HybridClient ([fbce72b](https://github.com/Kehl-io/nestweaver/commit/fbce72baf3499aac5cbd88e66e593aa08f81b55a))
* **client:** HybridClient shell — wraps DaemonClient, pass-through when no upstreams ([64a37c5](https://github.com/Kehl-io/nestweaver/commit/64a37c5177849513fbf15ae0ab599dd1107c8791))
* **client:** inject provenance metadata in hybrid responses ([d283c1c](https://github.com/Kehl-io/nestweaver/commit/d283c1ca7e636b0b74b7e37cf454584c9cab77ec))
* **client:** merge routing mode with parallel queries ([8a0401c](https://github.com/Kehl-io/nestweaver/commit/8a0401c08b6facc33e96a8d515e5609d04c1668f))
* **client:** mode-aware adaptive upstream timeout ([6f0dd25](https://github.com/Kehl-io/nestweaver/commit/6f0dd25e6f244b276a6541e4b1fda14f85f57682))
* **client:** offline fallback with 30s background health checks ([31cfff7](https://github.com/Kehl-io/nestweaver/commit/31cfff7a3f34c0da7479a21549724ea16ae39afd))
* **client:** per-tool routing matrix — maps MCP tools to routing categories ([88b12a5](https://github.com/Kehl-io/nestweaver/commit/88b12a5984d0b1e77a54cc9406e5da3b0e69c2aa))
* **client:** scope-hash dedup — identity tuple (repo, file, symbol, scope_hash) ([68762bf](https://github.com/Kehl-io/nestweaver/commit/68762bfc7266e199b2f8ea4b310f049f52943725))
* **client:** trace stitching for cross-boundary flow_trace ([aff83d9](https://github.com/Kehl-io/nestweaver/commit/aff83d9f62396712510d49fbab0e584ef546aff7))
* **client:** two-tier blast_radius with local + org-wide sections ([5b7da7b](https://github.com/Kehl-io/nestweaver/commit/5b7da7bdc5ccdfeca21f160faead810e178fb6da))
* **client:** upstream discovery and server.toml parser ([383856e](https://github.com/Kehl-io/nestweaver/commit/383856e05f16d500e1ea94087c4a4b4d1ddde74a))
* **client:** UpstreamHandle with auth injection, health tracking, repo glob matching ([7170ab5](https://github.com/Kehl-io/nestweaver/commit/7170ab52a26bc770c8f25da7c3a9172c765bec8a))
* **client:** weighted RRF merge (k=60, 1.5x local weight) for hybrid results ([cf09fe1](https://github.com/Kehl-io/nestweaver/commit/cf09fe12c27eda2454650be8b114b52c8520cdca))
* **cli:** nestweaver connect command for upstream registration ([87e43ef](https://github.com/Kehl-io/nestweaver/commit/87e43ef8ecc946bb67a5b83da4136406bf658c5f))
* **cli:** nestweaver pre-push-impact --local-changes command ([be46f9d](https://github.com/Kehl-io/nestweaver/commit/be46f9d0c582b3367616e526194bf3167a07a5c1))
* **cli:** nestweaver server init-tls for certificate generation ([7083398](https://github.com/Kehl-io/nestweaver/commit/7083398d6c11935644c193b30008f98940a5ad3b))
* **cli:** server unavailability fallback with fail-on-error option ([587869f](https://github.com/Kehl-io/nestweaver/commit/587869f27e5658d52e7c56c0523716728c1129bd))
* **cli:** show upstream info in nestweaver status ([2762b07](https://github.com/Kehl-io/nestweaver/commit/2762b07a921436009bce79b5db6a35492712ab03))
* **contracts:** field-level OpenAPI breaking-change diff (contracts diff) ([756f5d5](https://github.com/Kehl-io/nestweaver/commit/756f5d5be39dbc79872f7c1603e92342fe320f64))
* **daemon,mcp,engine:** wire query-timeout/disconnect cancellation end-to-end ([469dcac](https://github.com/Kehl-io/nestweaver/commit/469dcac15afe11cff943dea04dd1c6fb4a676919))
* **daemon:** ACME module (tokio-rustls-acme, TLS-ALPN-01) + no-feature guard ([5c8f3ca](https://github.com/Kehl-io/nestweaver/commit/5c8f3cae20e2c88f5d0ec2d88c4d57084957de2c))
* **daemon:** ACME opt-in surface + treat ACME as TLS for the bind gate ([7e94f9d](https://github.com/Kehl-io/nestweaver/commit/7e94f9db840eb42c0397a1d55ede4fe53878b233))
* **daemon:** add --server CLI flags (parsed, no behavior) ([4fb166d](https://github.com/Kehl-io/nestweaver/commit/4fb166deb402c049968c128932d313b0f56e093f))
* **daemon:** add 'daemon gc' and stop launchd agents for temp DBs ([1929513](https://github.com/Kehl-io/nestweaver/commit/19295135f0b272d64ae3b699bc0e58b42a9072b0))
* **daemon:** add RepoStates RPC for staleness comparison ([00a537c](https://github.com/Kehl-io/nestweaver/commit/00a537c72428c654a0669adedd2d44826c4c9b55))
* **daemon:** admin REST API for server management ([e4084cc](https://github.com/Kehl-io/nestweaver/commit/e4084ccae3435df71b66061803c5dfc2a56ae209))
* **daemon:** bearer token auth interceptor for TCP transport ([c4cebd6](https://github.com/Kehl-io/nestweaver/commit/c4cebd6905da678bc8ce28fba38f52ed92d7520a))
* **daemon:** cooperative cancellation + timeout/disconnect abort for index_repo ([9c91cfc](https://github.com/Kehl-io/nestweaver/commit/9c91cfcd624a917e385dccdc6538132219e74842))
* **daemon:** expose indexing status in BrainStatus response ([a7683f4](https://github.com/Kehl-io/nestweaver/commit/a7683f47249c1e8cb884ef7ba96465ca49d74dea))
* **daemon:** FlowTraceContinue RPC for cross-boundary trace stitching ([d4ecaba](https://github.com/Kehl-io/nestweaver/commit/d4ecabad7e3ac90eebe45add5894ba57be5bc486))
* **daemon:** in-process Backup RPC holding the write lock; drop Prepare/Finish handlers ([e3643e2](https://github.com/Kehl-io/nestweaver/commit/e3643e2ed6bb85c896b4f20b365552e288edbc1e))
* **daemon:** per-client rate limiting via token bucket ([ace1d05](https://github.com/Kehl-io/nestweaver/commit/ace1d0535d865166497fad7a790e891c905e117d))
* **daemon:** Prometheus metrics endpoint ([9da395f](https://github.com/Kehl-io/nestweaver/commit/9da395f239bcaaaa7cb90e70a7f6f48e0b9e91c6))
* **daemon:** query safeguards with per-tool timeouts and depth limits ([0999047](https://github.com/Kehl-io/nestweaver/commit/09990476a90cd14c3ac0e2556141b25a8adfedb5))
* **daemon:** read-only mode + write-RPC guards (snapshot-replica foundation) ([5e6a59a](https://github.com/Kehl-io/nestweaver/commit/5e6a59aef7338bd6e6e40aa972aa13966c116875))
* **daemon:** read-only snapshot replica serving (daemon run --server --snapshot) ([90619ae](https://github.com/Kehl-io/nestweaver/commit/90619ae2b4b511c24e31e417cc915d892575193f))
* **daemon:** TCP listener alongside UDS in server mode ([4fe1bab](https://github.com/Kehl-io/nestweaver/commit/4fe1bab43f22050ca88c6e374e003a9777de6cea))
* **daemon:** verify Gitea webhook signatures ([2fb6d73](https://github.com/Kehl-io/nestweaver/commit/2fb6d731c9341f6ea3fbe8e052144b2f316b991b))
* **daemon:** webhook endpoint with HMAC verification and dual-secret rotation ([9c0ba26](https://github.com/Kehl-io/nestweaver/commit/9c0ba26baa5f11ddc05d947b6505fdf9c3e595a0))
* **daemon:** wire ACME auto-TLS into both listeners (serve-loop integration) ([4b5779d](https://github.com/Kehl-io/nestweaver/commit/4b5779ddd73e631db0b67a7af4b4bf9d8046dc84))
* **docker:** Dockerfile and docker-compose for server deployment ([823f548](https://github.com/Kehl-io/nestweaver/commit/823f54850663bf931bdb398e3210ae6ca0f3e5fa))
* **embed:** default to thenlper/gte-base (768-dim, better retrieval) ([2b59490](https://github.com/Kehl-io/nestweaver/commit/2b594904f6383ff87e728d6b126f04c576fc6ac9))
* **embed:** support bearer auth for keyed remote embedding endpoints ([15ec1f8](https://github.com/Kehl-io/nestweaver/commit/15ec1f8713d834160d86c4deffbdfd5d8a305911))
* **engine,daemon:** cooperative cancel in index walk/parse + post-index phases ([3fd4663](https://github.com/Kehl-io/nestweaver/commit/3fd4663d16985b48e6bf968ea79a67a57592092a))
* **engine:** 2-hop transitive re-resolution for incremental updates ([b147393](https://github.com/Kehl-io/nestweaver/commit/b147393c46306e2ec1d77fcf7e598f7bef4fc0f0))
* **engine:** adaptive polling scheduler ([105895a](https://github.com/Kehl-io/nestweaver/commit/105895a008c0c97a164540565e18f4fd1390272d))
* **engine:** bare clone workspace management ([a518941](https://github.com/Kehl-io/nestweaver/commit/a518941a84b03558541db7729ae90f78b03ba4f1))
* **engine:** Chianti-style atomic change diffing ([4c62d27](https://github.com/Kehl-io/nestweaver/commit/4c62d27668734fee117510b74fef12be05d3c8aa))
* **engine:** circuit breaker per remote host for git fetch operations ([696bfae](https://github.com/Kehl-io/nestweaver/commit/696bfaedd514ef4dc4e7d6d3e9220b5d66861576))
* **engine:** compute canonical_id from scope chain at index time ([b622b59](https://github.com/Kehl-io/nestweaver/commit/b622b591f4312bce96e9d16dd6ef67a71c55efa5))
* **engine:** define ContentReader trait and FilesystemReader backend ([3a73139](https://github.com/Kehl-io/nestweaver/commit/3a7313957b94633c0c6eb804744d3e8626153214))
* **engine:** GitBareReader — ContentReader backend for bare git clones ([7cce0de](https://github.com/Kehl-io/nestweaver/commit/7cce0de9ffa2e7f3272bea2c07fae35339baf56a))
* **engine:** impact analysis with BREAKING/WARNING/INFO classification ([be29ee2](https://github.com/Kehl-io/nestweaver/commit/be29ee22c6123ebee903d5d6a298c10c15cc4a77))
* **engine:** materialize_snapshot — compat-gated private working copy for replicas ([4e49ac9](https://github.com/Kehl-io/nestweaver/commit/4e49ac92187c8e2ccb40aadee5516edb1c8321e4))
* **engine:** periodic full re-index with proportional threshold ([583635d](https://github.com/Kehl-io/nestweaver/commit/583635daaee4f02162248da665f2d9a09edc0aa3))
* **engine:** precomputed hub-node summaries (SummaryLevel::Hub) ([208ec7a](https://github.com/Kehl-io/nestweaver/commit/208ec7ae7d276dbef956ae3cd069a51086f29b4c))
* **engine:** serve agent-guide hubs from the generation-gated summary cache ([c8f217c](https://github.com/Kehl-io/nestweaver/commit/c8f217cd9c3cabdda5b335adcc36ee4fc8b5a9ec))
* **engine:** server-mode incremental and periodic-full reindexing ([b9ab69d](https://github.com/Kehl-io/nestweaver/commit/b9ab69d00fe5c2eb57d75fc9838892e94d795f90))
* **engine:** server-side read_symbols and regex_search behavior ([61c338f](https://github.com/Kehl-io/nestweaver/commit/61c338f1c6cab3fb681efeb45d898333138142a8))
* **engine:** SQLite-backed job queue for server indexing ([8bd0773](https://github.com/Kehl-io/nestweaver/commit/8bd0773378680bc9f00c18e721591f3b412da2fb))
* **engine:** support type="vault" repos and default to 8 index workers ([7481996](https://github.com/Kehl-io/nestweaver/commit/748199629c04a8e383d99f12a0f731d86748cee0))
* **engine:** transactional incremental index path for crash safety ([bf64dca](https://github.com/Kehl-io/nestweaver/commit/bf64dca10b24d29f694fb7379d89e886685958aa))
* **engine:** worker pool for server-mode indexing ([55c7fec](https://github.com/Kehl-io/nestweaver/commit/55c7fecaf27ba0abebb6ed6ec2df12ecdc6ee897))
* **engine:** working-tree diff for pre-push atomic changes ([dc67cce](https://github.com/Kehl-io/nestweaver/commit/dc67cce26e840eed13dc6ecf692d0cdff9cd07e2))
* hybrid server architecture (server mode, blobless indexing, client federation, CI integration) ([a683559](https://github.com/Kehl-io/nestweaver/commit/a683559fe05c7bbb99b9e24ea8f748b9c9ffabc3))
* **mcp,daemon:** federate two-tier results and staleness at the daemon /mcp boundary ([76c315f](https://github.com/Kehl-io/nestweaver/commit/76c315f5b4f875b46e838e5503ab83da0de5777f))
* **mcp:** expose server_mode in brain_status output ([32ade78](https://github.com/Kehl-io/nestweaver/commit/32ade78e112d507c602be74b3ca9c305713f72c9))
* **mcp:** full tool dispatch over HTTP ([91fb8b9](https://github.com/Kehl-io/nestweaver/commit/91fb8b9d2a423e6f082dc49e3fbf92261c719f42))
* **mcp:** HTTP endpoint with tools/list and initialize support ([ebc2c22](https://github.com/Kehl-io/nestweaver/commit/ebc2c22150a29ee2e76a07e1d5cc1aaa1f0c5b4d))
* **mcp:** session registry with DashMap and expiry ([93e28e3](https://github.com/Kehl-io/nestweaver/commit/93e28e3ce5341d1bb5243f9e66f0fa45b99bc7cf))
* **parser:** scope-chain extraction for all 32 supported languages ([a09f4e3](https://github.com/Kehl-io/nestweaver/commit/a09f4e31ac0f6b3a90f252d6418b50b3b74c3cdb))
* **project_context, mcp:** concise default + filters; /mcp boundary provenance (nw-017, research-backed) ([4efa1bb](https://github.com/Kehl-io/nestweaver/commit/4efa1bbe82fb295bad8dfa02015f12d974296cf3))
* **schema,store,engine:** decouple repo identity (git origin) from on-disk location (root_path) ([d32f29e](https://github.com/Kehl-io/nestweaver/commit/d32f29e7a224e9543aaaf245678d10bae3a25250))
* **schema:** canonical_symbol_id with scope-chain hash ([b36bdc8](https://github.com/Kehl-io/nestweaver/commit/b36bdc89483b4921f4d3ab800f4d378fd9aa9725))
* **store,engine:** cancellable vector-search primitives (foundation for query cancel) ([a72b91c](https://github.com/Kehl-io/nestweaver/commit/a72b91cf84ef11b16230d46417e4ce109523562c))
* **store:** add canonical_id field to Symbol nodes with ALTER TABLE migration ([5faf07b](https://github.com/Kehl-io/nestweaver/commit/5faf07b11cc7ed6707ea22c8d7bfe4e237a2640a))
* **symbol:** wire --instance into scoped resolution (nw-016, research-backed) ([ba35b5f](https://github.com/Kehl-io/nestweaver/commit/ba35b5f228c7af66b6cb37d70d9cdf3c39a39299))
* upstream RoutingMode (primary/merge/fallback) now overrides per-tool routing ([cf09905](https://github.com/Kehl-io/nestweaver/commit/cf099052da02079a498b4d6864a0486a016236ec))
* use upstream repo globs for routing instead of always picking first healthy ([ff72006](https://github.com/Kehl-io/nestweaver/commit/ff720061ec043c89f682058d5c47824fa19d1c54))
* **web:** admin dashboard pages for server management ([0d7edac](https://github.com/Kehl-io/nestweaver/commit/0d7edac7ed4da14a46f192b715dd0a79b04060bd))
* **web:** device-flow auth endpoints and complete IPv6 SSRF validation ([412a1b7](https://github.com/Kehl-io/nestweaver/commit/412a1b721e6e466d7104340b4e6927dc22960146))
* wire [[upstream]] from instance.toml into upstream discovery ([25bbc31](https://github.com/Kehl-io/nestweaver/commit/25bbc316b8831969834302d84b893d5d435f9e0d))
* wire server.indexing config and per-repo branch/poll to RepoConfig ([d8c89b6](https://github.com/Kehl-io/nestweaver/commit/d8c89b6fd16d46b880b2609b293e1eb00f118ca8))


### Bug Fixes

* --json daemon/direct parity + depth-clamp test (nw-016/nw-018) ([cceeeb4](https://github.com/Kehl-io/nestweaver/commit/cceeeb42f50005f63b81d1520cdd51d0e165222d))
* **#2,#4:** contracts --json on daemon path; reload_config parse-once ([92fa039](https://github.com/Kehl-io/nestweaver/commit/92fa0395f1cdd033a3f1822101a314bf2d1c397d))
* 3 bugs from final hardening round (embed no-daemon, bare-cap, auth guard) ([7613c46](https://github.com/Kehl-io/nestweaver/commit/7613c4651721d25f19c96ecabf921ea9e504b379))
* add --config flag to daemon run for container/foreground mode ([7d4f6b5](https://github.com/Kehl-io/nestweaver/commit/7d4f6b5ab131fbb29b33c45abe5c8e5e050cc21e))
* address review findings and CI failures (clippy, AdminStatus, hostname auth, timeout cleanup) ([6df2468](https://github.com/Kehl-io/nestweaver/commit/6df2468dc1fc31b9fc0ce69d6236010cdb99916a))
* admin add/remove repos update live scheduler and webhook state ([401c209](https://github.com/Kehl-io/nestweaver/commit/401c2092a9a2bd861e70fc1c5e81058b6acf5989))
* admin queue/status reports actual running jobs from job queue ([f4bd09d](https://github.com/Kehl-io/nestweaver/commit/f4bd09dcd44d6c87c783545f89da1e68d7f79a0b))
* admin remove_repo acquires write mutex to prevent graph races ([189399f](https://github.com/Kehl-io/nestweaver/commit/189399f2b2edfb85dfca9b8741cd3a32a158a8bf))
* admin version inherits workspace, MCP caps max_depth ([6aa1886](https://github.com/Kehl-io/nestweaver/commit/6aa1886679c504256139bd9f249e2b1467978681))
* **admin:** config ordering, db_size_bytes, device auth approval UI ([6878efe](https://github.com/Kehl-io/nestweaver/commit/6878efe56212d371e9c423b6d0676f687a266e84))
* **admin:** dead-letter retry uses correct identifier ([eea4d6e](https://github.com/Kehl-io/nestweaver/commit/eea4d6e03b61c8f0280f1929207d396a15e386f2))
* **admin:** reload_config runs startup-style repo reconciliation ([7715505](https://github.com/Kehl-io/nestweaver/commit/77155052680f5a73544a5bf266c6c317e3f501f5))
* **admin:** show pending jobs and drain state in queue status ([bf019e2](https://github.com/Kehl-io/nestweaver/commit/bf019e25d1c0f46718c2f4bd2d14ee5d3602dd22))
* AdminState test initializer + cargo fmt ([1549dc0](https://github.com/Kehl-io/nestweaver/commit/1549dc027e14c4b54b73013343c6afb037c3c170))
* **admin:** upsert handles cancelled jobs so remove/re-add works ([c17a097](https://github.com/Kehl-io/nestweaver/commit/c17a0976f6114d12ce799941f7b36edbdcc1e681))
* **admin:** wire add_repo, trigger_reindex, retry_dead_letter endpoints ([29f6387](https://github.com/Kehl-io/nestweaver/commit/29f63874d836d4deebdc7a46b094c6d660574f3f))
* **admin:** wire dismiss_dead_letter and reload_config endpoints ([c3ec0e6](https://github.com/Kehl-io/nestweaver/commit/c3ec0e6e2f2f64dafe63105fb35e9567b1b4903f))
* align admin API response shapes with React dashboard expectations ([090b79a](https://github.com/Kehl-io/nestweaver/commit/090b79a29bc781e74a40cf4fca7567bfd2e4660f))
* align docs poll_min/poll_max with code min_poll/max_poll, add serde aliases ([c46fe47](https://github.com/Kehl-io/nestweaver/commit/c46fe47c5eb97f7e14c3de56842c1b5a8144ef56))
* **app:** launch the daemon via launchd (Aqua agent) for GPU access ([c06aed7](https://github.com/Kehl-io/nestweaver/commit/c06aed7d9293a89f735547687fdffde71f063f14))
* **app:** stop endless daemon respawn loop that spammed crash notifications ([ef422e2](https://github.com/Kehl-io/nestweaver/commit/ef422e2f7e3d45f5eb97edc0505a794bf5257463))
* **backup:** bare clone detection, partial restore recovery, force WAL warning, legacy checksum handling ([f7bdfe2](https://github.com/Kehl-io/nestweaver/commit/f7bdfe2616af6e9470c83dc84567d2b19793fdf6))
* **backup:** hold write quiesce through entire backup copy operation ([357e8ef](https://github.com/Kehl-io/nestweaver/commit/357e8efd81201fe5fce20923abd6d97e6adc6e45))
* **backup:** populate manifest repo/symbol counts and compressed size ([a9e95d9](https://github.com/Kehl-io/nestweaver/commit/a9e95d9e553064a6d0e3bb6dc8bca21c89e32bd1))
* **backup:** recover from a leftover .restoring dir instead of deleting it ([92be8de](https://github.com/Kehl-io/nestweaver/commit/92be8de7d6ca4e3eb1c406fcf10788568ae1ecda))
* **backup:** rename clones/ to workspace/ on restore for daemon compatibility ([6a7bee3](https://github.com/Kehl-io/nestweaver/commit/6a7bee36408a76f45ae038a125dcf99bc1bfe084))
* **backup:** restore fails closed on an unreadable pidfile ([37da086](https://github.com/Kehl-io/nestweaver/commit/37da08615b94d564ba0c5d9868a6cc700f9fee0f))
* **backup:** restore refuses to run against a live daemon ([66a520e](https://github.com/Kehl-io/nestweaver/commit/66a520e97f66876abc4e92d437dc9dcb5bc66c16))
* **backup:** route through daemon to avoid lock conflicts ([b8d7b79](https://github.com/Kehl-io/nestweaver/commit/b8d7b795a64d46e5db661c1374ee6abfb10f2f5b))
* **backup:** verify checksums on inspect, atomic restore with cleanup ([36878ab](https://github.com/Kehl-io/nestweaver/commit/36878ab5cd84b26d3e6e6fad1f987f4c392906fa))
* bare-clone branch refs, webhook branch, CLI --config, timeout cap, stale docs ([236dfe6](https://github.com/Kehl-io/nestweaver/commit/236dfe64989649ed02f2783c7bb72ae13404dd65))
* blobless bare clone (--filter=blob:none) + spawn PollScheduler from daemon ([19e7491](https://github.com/Kehl-io/nestweaver/commit/19e74918b8f92308b9032d0c5c5502e5418cd330))
* bug-hunt fixes — empty-project resolution + project_context concise parity ([e2e74ae](https://github.com/Kehl-io/nestweaver/commit/e2e74aeb29b0e2ddefb862a484a14faa1a093d2c))
* **build,daemon:** wire acme feature to the binary; no-feature bind fails closed ([297f789](https://github.com/Kehl-io/nestweaver/commit/297f789525f0a387471a05c7f7a1fdc8ec15c773))
* cancel_repo uses status marker instead of DELETE to prevent ID reuse ([23e0af2](https://github.com/Kehl-io/nestweaver/commit/23e0af2417f1c0a89d5f411a5ec4da325dbf9828))
* CI action guards, Docker jq and comments, example env tokens ([afda4a2](https://github.com/Kehl-io/nestweaver/commit/afda4a27c371336e6c658ac95d6c95a6f20c8000))
* **ci,impact:** address 3 PR-review findings in the impact action + comment ([158ec6e](https://github.com/Kehl-io/nestweaver/commit/158ec6efe10939738d55584bc4ba6561fd6fd388))
* **ci:** correct command name and severity string in CI templates ([8ac7a37](https://github.com/Kehl-io/nestweaver/commit/8ac7a37a2e7f0494ad5808acda65c22b4a450176))
* **ci:** correct format-comment command in GitLab template ([6cc2f58](https://github.com/Kehl-io/nestweaver/commit/6cc2f5886e4131a208ffd0fc77e8213f49b177a1))
* **ci:** distinguish server error from clean analysis in GitLab template ([cd7ae5b](https://github.com/Kehl-io/nestweaver/commit/cd7ae5b02cb1f2cfdcd9742345ee2d7d9b28d584))
* **ci:** fail-closed comment dedup with bounded retry ([5d739c0](https://github.com/Kehl-io/nestweaver/commit/5d739c0073502379c94cdd5c7689c6a5ef57c5b7))
* **ci:** NUL-delimited diff parsing handles non-ASCII paths ([21e8986](https://github.com/Kehl-io/nestweaver/commit/21e8986023bcece975856549c3f101be9540976a))
* **circuit-breaker:** re-arm cooldown on failed half-open probe + single-probe gate ([f5357be](https://github.com/Kehl-io/nestweaver/commit/f5357be140129cf045014b8326b2778240851436))
* **circuit-breaker:** release probe permit if guarded closure panics ([b86ec9c](https://github.com/Kehl-io/nestweaver/commit/b86ec9c0c8088aad5d8e80dff36009bec7f9c9b7))
* **cli,mcp:** not-found/ambiguous status protocol for daemon impact (nw-016) ([9762ca8](https://github.com/Kehl-io/nestweaver/commit/9762ca88fdd4bcfa6ca197825b56ccf79b037d1a))
* **cli:** add 'ppi' as visible alias for 'pre-push-impact' ([2473d56](https://github.com/Kehl-io/nestweaver/commit/2473d56413d7a6fd2110b9dc3a8ac807d62d1eb4))
* **cli:** backup restore --start actually starts daemon ([bcea620](https://github.com/Kehl-io/nestweaver/commit/bcea620cb322679da05f13dfa2629694e2a4c9bd))
* **cli:** backup restore --start detaches daemon stdio ([5405e29](https://github.com/Kehl-io/nestweaver/commit/5405e29f3042dc6876f2b62281548ca6625d5312))
* **cli:** daemon stop grace accommodates the drain ceiling ([f487f41](https://github.com/Kehl-io/nestweaver/commit/f487f416735fb5ff9d4b8ef80911d5a1edb502bf))
* **cli:** daemon stop grace window widened to 60s (env-overridable) before SIGKILL ([7ec4274](https://github.com/Kehl-io/nestweaver/commit/7ec4274955a4d8723cc1e0c71161b048e1ad35d9))
* **client:** active health re-probe restores downed upstreams ([078c640](https://github.com/Kehl-io/nestweaver/commit/078c64006c4a093426eadeda5f20169c8cd60353))
* **client:** auto-spawned daemon uses fork path, not a leaked launchd agent ([b274747](https://github.com/Kehl-io/nestweaver/commit/b274747bf6fd0d267f7072146f708730cffbbe24))
* **client:** bounded ejection backoff and cap concurrent upstream ejections ([3722526](https://github.com/Kehl-io/nestweaver/commit/37225268681044ecf43a21c4b4ad916717bc7988))
* **client:** dedup handles all result field naming conventions ([e58c6f3](https://github.com/Kehl-io/nestweaver/commit/e58c6f3f0305c03789d31a768260f88362b28d39))
* **client:** dedup merged results on normalized repo identity, not raw URL ([d8b6a4a](https://github.com/Kehl-io/nestweaver/commit/d8b6a4a5902ed8b2c75bf9c77bc4dc7bdf826779))
* **client:** exclude locally-resolvable repos from flow-trace boundary detection ([59275ac](https://github.com/Kehl-io/nestweaver/commit/59275ac716ace491ae686c52fa95d6101f845cb4))
* **client:** FanOut concatenates rows instead of symbol-deduping (nw-018) ([c4addd7](https://github.com/Kehl-io/nestweaver/commit/c4addd744c9bfcc1d361560fe663954e8e315d32))
* **client:** harden routing error semantics + timeouts + autostart race (nw-015) ([2f58f09](https://github.com/Kehl-io/nestweaver/commit/2f58f0966c8ab1920cbb941f1ad211cf09ca5afd))
* **client:** improve hybrid flow trace routing ([3a0a714](https://github.com/Kehl-io/nestweaver/commit/3a0a714724f98183059e0bccc3624110c25087ff))
* **client:** inject bearer auth into upstream routed queries ([1ea9861](https://github.com/Kehl-io/nestweaver/commit/1ea9861aba9a6ba1c429d89a1ccf6bd0bb69c479))
* **client:** merge structured responses in fallback and populate stale_repos ([2dc03b8](https://github.com/Kehl-io/nestweaver/commit/2dc03b8bded3a7b30afd385d854d5a3dea31eef7))
* **client:** refresh staleness in the background, not per query ([10bce8d](https://github.com/Kehl-io/nestweaver/commit/10bce8d201d3649cdab4882d5958a49d4721ada3))
* **client:** replace timestamp-only trace IDs with unique IDs ([d2f16a4](https://github.com/Kehl-io/nestweaver/commit/d2f16a4d78a8426d89b710d3f4e5f37a0f5bb109))
* **client:** RRF merge accumulates duplicate contributions and sorts deterministically ([4d2244f](https://github.com/Kehl-io/nestweaver/commit/4d2244fe90e81a8ba6a3caf055c9c49b4c9ce6c6))
* **client:** two-tier impact dedup matches on instance-independent repo identity ([533393a](https://github.com/Kehl-io/nestweaver/commit/533393a46af442dff561df10845a1af23c690af0))
* **client:** upgrade fallback upstream-failure log to warn level ([e8e98ad](https://github.com/Kehl-io/nestweaver/commit/e8e98adfe3832177b01c128f1543f51059fcd8d2))
* **client:** use repo_uid for two-tier dedup and populate parent_span_id ([f5e3ad8](https://github.com/Kehl-io/nestweaver/commit/f5e3ad864d8af8747de1c20a4eee3863d20eb82f))
* **cli:** keep pre-push-impact JSON clean and verify daemon start ([fec93cb](https://github.com/Kehl-io/nestweaver/commit/fec93cb7a5abc7e6c87499533b401ab8df5060e0))
* **cli:** refuse snapshot build while a daemon holds the DB (no torn snapshots) ([59e7ad3](https://github.com/Kehl-io/nestweaver/commit/59e7ad38e950d109e8e554e04a7efec9b47c3613))
* **cli:** route all read commands through HybridClient ([4a8bf09](https://github.com/Kehl-io/nestweaver/commit/4a8bf09334acf4205ec9fb68f9caedff8844b7a7))
* **cli:** route read commands through HybridClient ([5dc7c02](https://github.com/Kehl-io/nestweaver/commit/5dc7c02d58aa37a2ce62f6b658e210292cd89dbf))
* **cli:** snapshot build derives quiesce guard from db path, no autospawn ([bb554af](https://github.com/Kehl-io/nestweaver/commit/bb554af376e817360c9d85f9fd160e9b85510285))
* **cli:** warn when backup runs with active daemon ([2f3c5c2](https://github.com/Kehl-io/nestweaver/commit/2f3c5c2d8ac30ac2aedbe75d365f2c215cfdb9a7))
* Combined tools query upstream for enrichment, preserving object shape ([412b6e2](https://github.com/Kehl-io/nestweaver/commit/412b6e2faefc3d42e825019ce1d116a0d4524d78))
* complete() no longer sets updated_at, preventing false requeue loops ([d6093bb](https://github.com/Kehl-io/nestweaver/commit/d6093bb042fd0341577bdab0f01730dd3b6b8329))
* **config:** remove_repo must not swallow sections after a [[repos]] block ([003e6a1](https://github.com/Kehl-io/nestweaver/commit/003e6a19b7ec6f8e2a9c9dd726b69eb81ddf9d08))
* **contracts,config:** comment-safe class scan; TOML-header block terminator ([bdcbff7](https://github.com/Kehl-io/nestweaver/commit/bdcbff75d7a7af6da1a324817b4ec44b6c4a8f18))
* **contracts,web:** suppress $ref path-item false-BREAKING; saturating context add ([16711dd](https://github.com/Kehl-io/nestweaver/commit/16711dd1735d3f7bcf966d108cbdcc876dafba0d))
* **contracts:** don't treat a method-level @RequestMapping as the class base path ([c15ceb6](https://github.com/Kehl-io/nestweaver/commit/c15ceb66311a196216f3edb53fce6804ee679c49))
* **contracts:** parse Spring annotation path args (value=/array forms) ([aa4f09f](https://github.com/Kehl-io/nestweaver/commit/aa4f09fdd959bdea5fa488c6c2746c9ff2b306a4))
* **daemon,embed:** GPU embedding in the daemon + per-DB model selection ([58e9c9f](https://github.com/Kehl-io/nestweaver/commit/58e9c9f5f5640bb2debca4ba880226312bc5e8c3))
* **daemon,mcp:** propagate DB query errors instead of silent empty (nw-014) ([bbba2ff](https://github.com/Kehl-io/nestweaver/commit/bbba2ffe385ed3e4ca71daa94c7f2e6da26ae8bc))
* **daemon:** ACME failure falls back to TLS (self-signed), never cleartext ([b594a57](https://github.com/Kehl-io/nestweaver/commit/b594a570bb47144fa156ef84cb75317c237c730a))
* **daemon:** adaptive polling, worker default, token length, msgpack export guard, vault wiring ([e2204d4](https://github.com/Kehl-io/nestweaver/commit/e2204d4f4885eea6f38e5d413818e86480cc7422))
* **daemon:** add secure_eq helper for constant-time token comparison ([188e447](https://github.com/Kehl-io/nestweaver/commit/188e447b8f143576f52c3afc9cd9b054d60d7d4d))
* **daemon:** add webhook body limit and handle TLS error in spawn ([4bfa668](https://github.com/Kehl-io/nestweaver/commit/4bfa668bc4a9c4a845baac1e77bd8ea362993eb8))
* **daemon:** apply TLS to MCP HTTP server when --tls-cert is set ([0b2265c](https://github.com/Kehl-io/nestweaver/commit/0b2265cfcd00888b62e57a009703cf75a017dd7b))
* **daemon:** await the worker pool on shutdown so in-flight writes finish ([f45dead](https://github.com/Kehl-io/nestweaver/commit/f45dead4a88aa0a5e75f80b1b51e20bed609b9b1))
* **daemon:** bind + serve the socket before loading the embed model ([76700ff](https://github.com/Kehl-io/nestweaver/commit/76700ffea948b65da188b8d56b0509592080f891))
* **daemon:** block export_graph file writes in server mode ([aebe458](https://github.com/Kehl-io/nestweaver/commit/aebe458ac8d762bc3689fff82bb66d6e86b39134))
* **daemon:** cap in-flight TLS handshakes and stop accept tasks on shutdown ([7a27fc0](https://github.com/Kehl-io/nestweaver/commit/7a27fc074d088d858c340feb22b0d74e86c7ffb7))
* **daemon:** claim instance flock before snapshot materialization ([ce08ca2](https://github.com/Kehl-io/nestweaver/commit/ce08ca2f8c356db8b9ba359b44398dea7337cdda))
* **daemon:** client disconnect cancels in-flight query via drop-guard ([b03bf1f](https://github.com/Kehl-io/nestweaver/commit/b03bf1f4b84a6e48d8532dc205c16b6d8291061c))
* **daemon:** constant-time comparison for GitLab webhook tokens ([fc04013](https://github.com/Kehl-io/nestweaver/commit/fc04013640d1944779a74fe102eaba3e72ff6f1e))
* **daemon:** drain actually stops worker and polling ([adf5669](https://github.com/Kehl-io/nestweaver/commit/adf56696f4c503eb0bf39f773d622abe23e242ad))
* **daemon:** enforce admin-only access on destructive RPCs ([f4f14a6](https://github.com/Kehl-io/nestweaver/commit/f4f14a64e4423385dad84a238cd0d0bcde5db132))
* **daemon:** error when only one of --tls-cert/--tls-key provided ([40d5d0e](https://github.com/Kehl-io/nestweaver/commit/40d5d0e0d662177c4b213d7d6b5e92346b9eb83c))
* **daemon:** gc probes pidfile flock — spare live daemons, reap crash-loopers ([d4580f7](https://github.com/Kehl-io/nestweaver/commit/d4580f715b86416c2c94ac05c7f3a3b8d41b7c46))
* **daemon:** grant UDS callers admin access, migrate legacy instance IDs on upgrade ([172281e](https://github.com/Kehl-io/nestweaver/commit/172281e36d392e32a254beb1115efee4bb2d2123))
* **daemon:** guard idle_timeout in server mode + wire poll counters ([2a038c9](https://github.com/Kehl-io/nestweaver/commit/2a038c9a2450acd0604c3086b73e3da28b6951bb))
* **daemon:** harden is_unsafe_index_root against macOS firmlink + case bypass ([6adb83a](https://github.com/Kehl-io/nestweaver/commit/6adb83a516653d45f183c0d1f705d45f9d30dfd7))
* **daemon:** idle timeout must not fire during an active index ([f889c81](https://github.com/Kehl-io/nestweaver/commit/f889c81531e68f44a664028e5604e5b18305767b))
* **daemon:** install file logger + cap embed-model load ([9639f53](https://github.com/Kehl-io/nestweaver/commit/9639f53446b4624d3eccb730405d2c6e93fbd53a))
* **daemon:** key rate limits by peer identity and enforce a webhook secret minimum ([f31669e](https://github.com/Kehl-io/nestweaver/commit/f31669ea325ea002fe25704ceb4317597041faaf))
* **daemon:** mount admin and metrics routes on server-mode HTTP router ([a1d3e75](https://github.com/Kehl-io/nestweaver/commit/a1d3e753ae70b1d475961cd2c074801c52b4dffa))
* **daemon:** per-connection TLS handshake timeout; atomic ACME cache writes ([60f602f](https://github.com/Kehl-io/nestweaver/commit/60f602f3f61f97c1988b39d0262eb9f2fa482c10))
* **daemon:** per-replica private working dir so co-located replicas don't clobber ([e340acf](https://github.com/Kehl-io/nestweaver/commit/e340acf6ca6daa669925bdd31755ece76a15ecc3))
* **daemon:** poll ls-remote runs off the runtime with a timeout and status check ([edb7687](https://github.com/Kehl-io/nestweaver/commit/edb76872afd70a686e567ce21593c4c6da467e36))
* **daemon:** propagate JobQueue::open error instead of panicking ([7588228](https://github.com/Kehl-io/nestweaver/commit/7588228e65572983bf8a3056729c06a3336af771))
* **daemon:** read-only replica does not mount webhook/job/admin-write surfaces ([610712e](https://github.com/Kehl-io/nestweaver/commit/610712eceaa4358d9c5806bca968a2e7f4bc499b))
* **daemon:** refuse to index a system root (prevents whole-disk walk + CPU peg) ([ec9b01a](https://github.com/Kehl-io/nestweaver/commit/ec9b01a889a75dd3f092b7a03e5223d7783a2a5e))
* **daemon:** reindex_search holds the write gate; tantivy rebuild is atomic ([769360c](https://github.com/Kehl-io/nestweaver/commit/769360c06eb762132105df6e747d9da89e42d6e7))
* **daemon:** reject all mutating RPCs at a single read-only chokepoint ([d4e83b7](https://github.com/Kehl-io/nestweaver/commit/d4e83b7ab5f3464e0c131981ab818e181193bd0f))
* **daemon:** seed polling from config repos with branch and poll overrides ([26a745a](https://github.com/Kehl-io/nestweaver/commit/26a745a9c62b7d4c0fb0567e033dbdfe7438295c))
* **daemon:** set drained at shutdown start so the worker stops claiming new jobs ([88d0aa9](https://github.com/Kehl-io/nestweaver/commit/88d0aa9f3fa3f0f854c63553e377128c267d9268))
* **daemon:** set_extension writes hold the write gate ([3d3ff60](https://github.com/Kehl-io/nestweaver/commit/3d3ff601928154f8a67542781e6a82bab9e00e7d))
* **daemon:** share JobQueue across webhook and worker to prevent crash ([787d456](https://github.com/Kehl-io/nestweaver/commit/787d456c8c05c1ab81c313f06d4c548811880f59))
* **daemon:** shutdown drain waits on indexing_active, not just active_writes ([385774d](https://github.com/Kehl-io/nestweaver/commit/385774df3275aa452fb3f3b98b615b58267b538a))
* **daemon:** spawn WorkerPool to consume index jobs ([2b47de7](https://github.com/Kehl-io/nestweaver/commit/2b47de776378e64b29a935b253dd48f915b8b4a1))
* **daemon:** validate and restrict watch_vault/watch_code paths ([2ae49a2](https://github.com/Kehl-io/nestweaver/commit/2ae49a2473c630024af2a84ddea17659c2917c44))
* **daemon:** validate TLS config before binding ports ([cbafe95](https://github.com/Kehl-io/nestweaver/commit/cbafe951893884016b0f4cb50374bae3329f8660))
* **daemon:** watcher embed writes hold the write gate (drain-visible, backup-safe) ([c4cbd96](https://github.com/Kehl-io/nestweaver/commit/c4cbd96a2e2450c50d461aa0eb531b59f2636dc9))
* **deploy:** docker-compose provisions TLS certs so the server boots ([be890ae](https://github.com/Kehl-io/nestweaver/commit/be890ae065cd1bfcd0aef223b990ae19e4f50785))
* device auth URL, missing admin gates, MCP tool allowlist, useAdminApi memoization ([b698c33](https://github.com/Kehl-io/nestweaver/commit/b698c3312223201f2494b84c9c3603d7ee5cb6c9))
* Docker CMD adds --db and --bind 0.0.0.0 for standalone use ([77b9ec7](https://github.com/Kehl-io/nestweaver/commit/77b9ec77cf9ba130b40afb29fb4b1d2053150bb4))
* Docker only exposes ports that daemon run --server starts ([22c9173](https://github.com/Kehl-io/nestweaver/commit/22c91736f899f5fdce800003ed11d3272f2bab2d))
* **docker:** trim build context and ship a valid server config ([a2408d2](https://github.com/Kehl-io/nestweaver/commit/a2408d27663e947847803a43925278133e045c3b))
* **docker:** update Rust version to 1.88 ([7c9ad3c](https://github.com/Kehl-io/nestweaver/commit/7c9ad3c1eb98fe81f17c4fc34c0fdbe8bad53793))
* docs docker-compose --config flag order and webhook env var name ([280de77](https://github.com/Kehl-io/nestweaver/commit/280de776c81596b9044b790c0199ee3a2618aff8))
* **embed,contracts:** guard dimension mismatch, response order, repo-name drift ([0311d8b](https://github.com/Kehl-io/nestweaver/commit/0311d8b3b1a7a8a859b858e58f95b9059f477e7e))
* **embed:** don't disable semantic search for remote-embedded indexes ([2ae55a6](https://github.com/Kehl-io/nestweaver/commit/2ae55a63e2ce9a3f95f18afb5ab3280bdb5988c6))
* enforce depth and result limits in server dispatch ([15c7b43](https://github.com/Kehl-io/nestweaver/commit/15c7b438f3e9b707deaf3f566535b353be8f1fe3))
* **engine:** atomic backup restore via rename dance ([2718eb2](https://github.com/Kehl-io/nestweaver/commit/2718eb26700ba620445d13d50bfec565655884ee))
* **engine:** batch unresolved wikilinks on the incremental/watcher path too ([dd2ffc0](https://github.com/Kehl-io/nestweaver/commit/dd2ffc043a4e1cbd9abde8c18b1ce034d3015cc6))
* **engine:** batch unresolved-wikilink inserts (was O(links) DoS) ([0f786eb](https://github.com/Kehl-io/nestweaver/commit/0f786ebc5176a42e0ca19413560202b3498c1b0e))
* **engine:** bound cat-file --batch reads and cap bare-clone blob size ([c0baae8](https://github.com/Kehl-io/nestweaver/commit/c0baae899ad6853c68ccb59484ccfcf611ac86d6))
* **engine:** bound local git one-shots with timeout + isolate cat-file batch ([7304742](https://github.com/Kehl-io/nestweaver/commit/73047421b802584a9e3349fb776d66eca523e5d9))
* **engine:** checksum sidecar files in backup manifest ([80e0cb0](https://github.com/Kehl-io/nestweaver/commit/80e0cb0b08c6ca8606a41d43cacac310231fd6cc))
* **engine:** classify optional parameter additions as Info, not Breaking ([e428d09](https://github.com/Kehl-io/nestweaver/commit/e428d09323b40f73507efe8010cd605347f9b20f))
* **engine:** document cross-repo edge limitation in resolver ([ae0f4ed](https://github.com/Kehl-io/nestweaver/commit/ae0f4ede59fe5f4d1a2c3a7bebbc8159c6a4d112))
* **engine:** enforce file-size cap on the local (filesystem) index path ([cb3b773](https://github.com/Kehl-io/nestweaver/commit/cb3b7736a9d77e6c4d92f5458d64b1229b62f856))
* **engine:** enforce SSRF allowlist at git clone/fetch/probe time (nw-007) ([6c80e60](https://github.com/Kehl-io/nestweaver/commit/6c80e60d9a5d32f4dbb205e91936f88559da567c))
* **engine:** fail-closed on DNS resolution and reject git:// scheme ([947baad](https://github.com/Kehl-io/nestweaver/commit/947baad4bf70c83d06748b23cb7d12c5132f5427))
* **engine:** generation-gate the summary sidecar (no stale summaries after reindex) ([003807a](https://github.com/Kehl-io/nestweaver/commit/003807ae237e96533656857be48470066126bb66))
* **engine:** git diff change detection handles non-ASCII paths ([397b47e](https://github.com/Kehl-io/nestweaver/commit/397b47eb7760fb08d66cd081f4f0b121c3eb494d))
* **engine:** git subprocesses time out and kill the child (no wedged fetch) ([5865237](https://github.com/Kehl-io/nestweaver/commit/5865237e576cb77069d66f735afeab3b6ce85002))
* **engine:** GitBareReader handles non-ASCII paths and skips symlinks/gitlinks ([e53648c](https://github.com/Kehl-io/nestweaver/commit/e53648c61779a0ff0f3d31c006a4bab052aee315))
* **engine:** hash-suffix bare-clone dirs to prevent same-basename collisions ([f175e73](https://github.com/Kehl-io/nestweaver/commit/f175e7336b4340be0c01cb1c1fe253ea0741e046))
* **engine:** inline symbol bodies use the active ContentReader in server mode ([d239854](https://github.com/Kehl-io/nestweaver/commit/d239854c6787f6452ceb68daaf430ae06ae7d934))
* **engine:** kill git's whole process group on timeout; split net vs clone timeouts ([7480f55](https://github.com/Kehl-io/nestweaver/commit/7480f5558214eccb0e538e089b3e8d2c54945d85))
* **engine:** load taxonomy aliases in server-mode indexer ([52e1df4](https://github.com/Kehl-io/nestweaver/commit/52e1df46b90c1383ad93f2c359b35363dd494a10))
* **engine:** persist vault indexing state ([27b2319](https://github.com/Kehl-io/nestweaver/commit/27b23198ff3c11f6043bb5e88ecea9ae02df3835))
* **engine:** populate affected fields for SignatureChanged impacts ([d8c358a](https://github.com/Kehl-io/nestweaver/commit/d8c358a7f6858d92456d0c6d2a3852c2bac1c1a2))
* **engine:** process bare-repo markdown files instead of skipping ([e51a4c1](https://github.com/Kehl-io/nestweaver/commit/e51a4c19a61dbb0bfed3af4464b776f98022d11d))
* **engine:** prune removed files on full reindex; re-resolve 2-hop dependents on incremental (nw-009, nw-008) ([f5bb401](https://github.com/Kehl-io/nestweaver/commit/f5bb4011f5ec3e8d78f74bc3779abf7e0bd56650))
* **engine:** remove unused variables in format_comment module ([05d5809](https://github.com/Kehl-io/nestweaver/commit/05d5809d570960feb3839ed456791b3e97141b2f))
* **engine:** skip paths with embedded newline in cat-file --batch ([7078560](https://github.com/Kehl-io/nestweaver/commit/7078560f667e36b855d1de9667eeca3fdc754165))
* **engine:** sort repos in format_comment for deterministic output ([1e7c455](https://github.com/Kehl-io/nestweaver/commit/1e7c45572cf7dea3fcdc5c541713545ec42acd7a))
* **engine:** use ContentReader for cross-domain note scanning ([d616a12](https://github.com/Kehl-io/nestweaver/commit/d616a12a2869fbb298d9d88c588e3daf994a3471))
* **engine:** validate URL scheme and reject private IPs in admin add_repo ([2e06750](https://github.com/Kehl-io/nestweaver/commit/2e06750f914308bd451fdf963486b45db5938c3b))
* **engine:** watcher adopts existing repo identity to prevent graph loss ([3f61db5](https://github.com/Kehl-io/nestweaver/commit/3f61db5d77b049a92e2ce7916c4eb3514cdd9927))
* **engine:** wire analyze_impact to actual graph traversal queries ([382e5db](https://github.com/Kehl-io/nestweaver/commit/382e5db23c5a52db2d6ccc0b2255cb3db9991751))
* fallback upstream mode uses LocalFirst instead of keeping Merge default ([86a9b15](https://github.com/Kehl-io/nestweaver/commit/86a9b15eca8336f295ce53e9688756f3bfbe192f))
* **federation:** bound the background staleness-refresh RPC with a timeout ([ffb37f3](https://github.com/Kehl-io/nestweaver/commit/ffb37f3102a56fb54d165149b02edcb5fcf365e9))
* **federation:** cycle-guard client-side trace subtree assembly ([9de85f7](https://github.com/Kehl-io/nestweaver/commit/9de85f78888e9421c3f78a185cd999c4d91ceea3))
* **federation:** fallback timeout cap honors the &lt;200ms budget ([4ed7b5e](https://github.com/Kehl-io/nestweaver/commit/4ed7b5eff5b59e51a9055dc2946d4d3f1ead3f87))
* **federation:** merge-mode local-failure fallback + accurate merged accounting ([8e0d6be](https://github.com/Kehl-io/nestweaver/commit/8e0d6bea2c37a85f56f2eea9b87c8c2bc62565b6))
* flow_trace includes repo_uid for boundary detection ([eb9053c](https://github.com/Kehl-io/nestweaver/commit/eb9053cf4e39ac14e33bd4f748418753fb7f753b))
* forward tool_name through TwoTier routing instead of hardcoding blast_radius ([9c4b35a](https://github.com/Kehl-io/nestweaver/commit/9c4b35af3688319fa015c7b781e734a5b67604cb))
* **graph:** report downstream cross-repo callers in impact analysis ([b79da8e](https://github.com/Kehl-io/nestweaver/commit/b79da8e2ac46e0e21209a24f86e526fa506119b7))
* gRPC admin gating, dedup cross-instance coalescing, SymbolMoved for renames, action install URL ([5e6b7e5](https://github.com/Kehl-io/nestweaver/commit/5e6b7e5cf5b74e3d7658e68829371b83f779adcb))
* harden review findings — canonicalize fallback, shutdown race, metadata JSON ([09a61d7](https://github.com/Kehl-io/nestweaver/commit/09a61d721e1c408f226baddb86f88afa68a02977))
* **hybrid:** add brain_search, brain_context, project_context, note_get, hub_nodes to dispatch ([c630826](https://github.com/Kehl-io/nestweaver/commit/c6308266d5a7360b67b76f81d898e8de9b12141a))
* **hybrid:** always query upstream when local results are empty ([52ac3cb](https://github.com/Kehl-io/nestweaver/commit/52ac3cbea706c9911a3b3163d74ac6e4339e5017))
* **hybrid:** inject provenance metadata in all query return paths ([42bc64f](https://github.com/Kehl-io/nestweaver/commit/42bc64f2a0756123289949a80a86ca46c6faf1a7))
* **hybrid:** org_wide_impact key, provenance injection, dedup identity, staleness ([514e46f](https://github.com/Kehl-io/nestweaver/commit/514e46fa8c9a080402d8577b1db68ebd43ef55e6))
* **hybrid:** wire org-wide blast_radius filtering to exclude local repos ([1d51811](https://github.com/Kehl-io/nestweaver/commit/1d5181167a6782a0aa99437728200a35c42da88a))
* **hybrid:** wire per-tool routing matrix into query dispatch ([38673ca](https://github.com/Kehl-io/nestweaver/commit/38673ca0093e3254c206c0d575cd983299c39b30))
* **impact:** classify dynamic-language param rename as breaking ([3392a1d](https://github.com/Kehl-io/nestweaver/commit/3392a1d0de8b91f63eb4efbc6bb0aa2f8da6845c))
* **impact:** classify SymbolMoved as Breaking per the PRD, not Warning ([fa7fb52](https://github.com/Kehl-io/nestweaver/commit/fa7fb52be941961c60c8f3d36b19eb5325ee9ff0))
* **impact:** depth-aware param parsing; required Option&lt;T&gt; param is Breaking ([fd9277f](https://github.com/Kehl-io/nestweaver/commit/fd9277f1fc94a4142cf5e54a6a41e94716d10fab))
* **impact:** language-aware breaking-change classification (fail toward breaking) ([f15120c](https://github.com/Kehl-io/nestweaver/commit/f15120ce6f104a32044264b5086802cbc233d7fb))
* **impact:** report all removals/additions when canonical_id collides ([74f0ee3](https://github.com/Kehl-io/nestweaver/commit/74f0ee34246900d96216899c8d5f688ce73771d8))
* include min_poll/max_poll in scheduler ReloadConfig so reload updates poll bounds ([11427d1](https://github.com/Kehl-io/nestweaver/commit/11427d13c680a02fb7ea764758b53a7512e34edc))
* **index:** make full re-index delete+insert atomic to prevent empty-repo reads ([108a3af](https://github.com/Kehl-io/nestweaver/commit/108a3af920466c28df8b305d90fc5e7ecf52c2f6))
* **index:** preserve outgoing edges from changed files during incremental indexing ([6710c40](https://github.com/Kehl-io/nestweaver/commit/6710c40108a8a019a1d44981073b8bb615b3e288))
* **jobs,daemon:** continuous lease reaper reclaims crashed in-flight jobs ([b3b2632](https://github.com/Kehl-io/nestweaver/commit/b3b26323af5052f7c0bc05ed3dcffe24a5aade98))
* **jobs:** allow re-enqueueing succeeded and dead_letter jobs ([707cc69](https://github.com/Kehl-io/nestweaver/commit/707cc695bfc33d9f421a63c3187332b1bf9170bd))
* **jobs:** guard fail/complete on running status and owner (no resurrecting cancelled jobs) ([2dbb638](https://github.com/Kehl-io/nestweaver/commit/2dbb638cf9a5fe9032a9c4b829e29b39069e27d3))
* **jobs:** webhook priority preservation, remove dead code ([ec53bef](https://github.com/Kehl-io/nestweaver/commit/ec53befff5091ae24c2769205beb0e1c04087293))
* **mcp,cli:** cap dead_code output so it can't blow an agent's context window ([1936d7e](https://github.com/Kehl-io/nestweaver/commit/1936d7e2d9f950b67135fca698fb35c10267cc83))
* **mcp,engine,store:** cooperatively cancel dead_code/impact/flow_trace walks on timeout ([cdea390](https://github.com/Kehl-io/nestweaver/commit/cdea390bdc084a9ca80399fcf9b523336f90a69e))
* **mcp:** apply rate limiting and depth caps to MCP-over-HTTP ([76c0247](https://github.com/Kehl-io/nestweaver/commit/76c0247d4cc8d69a181b7be89587e2bf9b831021))
* **mcp:** document server-mode response shapes in tool descriptions ([5eca388](https://github.com/Kehl-io/nestweaver/commit/5eca388b571753db4d9772156e6b65f9371d4fcf))
* **mcp:** evict idle rate-limiter buckets and stabilize the initialize key ([3e517a8](https://github.com/Kehl-io/nestweaver/commit/3e517a803ef93d0dc32b83db3f7bf7cc742e9411))
* **mcp:** never cache a cancelled or incomplete query result ([d2b8c9c](https://github.com/Kehl-io/nestweaver/commit/d2b8c9cd90189ab260a37995d5d969938dca4b7c))
* **mcp:** pass embed_model through HTTP dispatch and enforce session rate limit ([7c95158](https://github.com/Kehl-io/nestweaver/commit/7c951589a2fc3a185c26b954444eec13ac0fd968))
* **mcp:** read_symbols groups targets by repo; regex_search uses store ([0808153](https://github.com/Kehl-io/nestweaver/commit/080815310e7dca334b2b8d09f24ba36fafd03c98))
* **mcp:** read_symbols uses GitBareReader in server mode ([4911e42](https://github.com/Kehl-io/nestweaver/commit/4911e42a83cf6af3a5c50417cda2260294e5588e))
* **mcp:** reconcile federated /mcp provenance to one honest source of truth ([10cedc5](https://github.com/Kehl-io/nestweaver/commit/10cedc5ec464c7c4c17052fe0ef2a0040bed66e3))
* **mcp:** redirect regex_search to Tantivy FTS in server mode ([f60f94e](https://github.com/Kehl-io/nestweaver/commit/f60f94ed757eae8d4c6818d90073489be15c95f8))
* **mcp:** regex_search server-mode uses brain_search fallback ([4cb52cf](https://github.com/Kehl-io/nestweaver/commit/4cb52cf7fdc8a384cc031f2fa1631c3540d7ac59))
* **mcp:** reject mutating tools on a read-only replica before dispatch ([ddc253d](https://github.com/Kehl-io/nestweaver/commit/ddc253d9ebbc6c0b7b1159473f84e250ed1d5d1b))
* **mcp:** reject unknown session IDs with re-initialize prompt ([a7da390](https://github.com/Kehl-io/nestweaver/commit/a7da3907e2f583ebc2fb50d9f53340506c51e84e))
* **mcp:** reload Tantivy reader after reindex for fresh brain_search ([9c08cd2](https://github.com/Kehl-io/nestweaver/commit/9c08cd205e339ede7c0a14c167b82a1d01aadb38))
* **mcp:** reuse constant-time admin check for rate-limit classification ([be7d4d9](https://github.com/Kehl-io/nestweaver/commit/be7d4d9f4fe34b74cc1ffde2932dd632a7114589))
* **mcp:** server_note shown for any empty-body result in server mode ([d15d390](https://github.com/Kehl-io/nestweaver/commit/d15d390c43f408397a802eb65ff55133f5a5adbe))
* **mcp:** thread server_mode through the MCP-over-HTTP transport ([87dce7f](https://github.com/Kehl-io/nestweaver/commit/87dce7f6411b369bf9881830b3e5beb71a542f4b))
* **metrics:** count every job completion, not just first-ever per repo ([7c9c7cf](https://github.com/Kehl-io/nestweaver/commit/7c9c7cfe3ba6d97c48e12337495742388ba4b5a8))
* **metrics:** init Prometheus metrics at startup and expose on admin port ([9b9bce0](https://github.com/Kehl-io/nestweaver/commit/9b9bce0a94a972d8f1c7cabd6a6dce51688b9c3a))
* **metrics:** instrument key paths to populate metric values ([751cff0](https://github.com/Kehl-io/nestweaver/commit/751cff0c257e2faed9f6a88470c01f46250ece71))
* mount admin API on web UI port so dashboard SPA can reach its backend ([3f2950a](https://github.com/Kehl-io/nestweaver/commit/3f2950a218626c00d97fab71d709fcbf152121e4))
* normalize repo identifiers in job queue to prevent duplicate/colliding jobs ([1f3c1cd](https://github.com/Kehl-io/nestweaver/commit/1f3c1cd6355f950ec16ef624622562e0598145cb))
* output parity + MCP robustness (nw-016/nw-017 partial) ([eefc929](https://github.com/Kehl-io/nestweaver/commit/eefc92996698524d35d8fe2c3f0f548c3ad014bd))
* pass --config through CLI hybrid paths for upstream discovery ([c58ab1b](https://github.com/Kehl-io/nestweaver/commit/c58ab1b438f0c4c3a6ad8dca27da2560bafd630e))
* preserve project metadata through structured merge for project_context ([375e000](https://github.com/Kehl-io/nestweaver/commit/375e000342b8c6bf6e6245fb294637809b826f2e))
* preserve structured schemas in hybrid merge for brain_context/project_context ([f5b361c](https://github.com/Kehl-io/nestweaver/commit/f5b361c5b627ef2ad506bb7024c1a6b0e19926ff))
* production-readiness — silent path failures, depth DoS, startup hang ([c2d1936](https://github.com/Kehl-io/nestweaver/commit/c2d1936f8d778fecfa94988b6a5d19d3ed097d0b))
* re-queue repos that received pushes during active indexing ([d021394](https://github.com/Kehl-io/nestweaver/commit/d0213946f3940affd0011e369562704cf09dbb53))
* reload updates webhook state, docs accuracy, NESTWEAVER_BIND env ([bec1393](https://github.com/Kehl-io/nestweaver/commit/bec13937830d4b10332838b22bb1dfe00f6c094d))
* requeue flag, Combined routing, connect TLS, queue purge, status, docs ([987a539](https://github.com/Kehl-io/nestweaver/commit/987a53962453c426e88adc24726cedb9a4d0f5ac))
* resolve first-party compiler warnings + cargo fmt ([9032d19](https://github.com/Kehl-io/nestweaver/commit/9032d1947539e254da1e0fd32cf15d0327aa940e))
* resolve remaining test warnings ([5a75131](https://github.com/Kehl-io/nestweaver/commit/5a751316dba75d77d4fef6236b4157d8cd069196))
* route CLI commands through HybridClient for upstream query support ([cfa923e](https://github.com/Kehl-io/nestweaver/commit/cfa923eb854480ab3f2afa5d2b12d41ad0ed7f19))
* route Combined tools through local-only to preserve object schemas ([87e0402](https://github.com/Kehl-io/nestweaver/commit/87e0402e496895fb8ed875b1d2e3adc9dd02620a))
* **safeguards:** communicate depth clamping in response metadata ([24192c7](https://github.com/Kehl-io/nestweaver/commit/24192c7d00e9455aa5c2366fc6deead0576d2ac7))
* scheduler removal uses configured repo name to match seeding logic ([542db29](https://github.com/Kehl-io/nestweaver/commit/542db29db094b816e3a59339cdf004492b233a04))
* **scheduler:** add 7-day time-based backstop for full re-index ([802911f](https://github.com/Kehl-io/nestweaver/commit/802911f565cd3fabe2f69a579c0111325088fb36))
* **schema:** canonical_id fallback includes line number to prevent collisions ([aaaf0c4](https://github.com/Kehl-io/nestweaver/commit/aaaf0c4877d242226ce6fb9a9a5ea41cbc0d6b38))
* **schema:** key local/file:// repos on full path, not basename ([506fb85](https://github.com/Kehl-io/nestweaver/commit/506fb856874eeab97504481272861f18dc37cffe))
* **schema:** normalize repo URL before hashing repo_uid so URL forms reconcile ([dd2a108](https://github.com/Kehl-io/nestweaver/commit/dd2a108b73c822103fbcaf23ba3484bec8d7de13))
* **schema:** stabilize canonical symbol IDs across line shifts ([94ee67a](https://github.com/Kehl-io/nestweaver/commit/94ee67a5352a005f830a5e2742808a78fb9cadd2))
* **security:** add bearer token auth to MCP-over-HTTP endpoint ([55bf89b](https://github.com/Kehl-io/nestweaver/commit/55bf89b2a2b0059254fa5f52306cc5f08e1f88ba))
* **security:** add brain_memory_consolidate to gRPC mutating tools gate ([473989d](https://github.com/Kehl-io/nestweaver/commit/473989d8f779e9249f0dcd749146a7add0985609))
* **security:** constant-time token comparison in MCP HTTP handler ([d40163c](https://github.com/Kehl-io/nestweaver/commit/d40163c822ed40ab7ff39d8e4d9947981b91e898))
* **security:** constant-time token comparison via subtle crate + remove fragile unwrap ([a05c202](https://github.com/Kehl-io/nestweaver/commit/a05c202e2644c7d65f43588d6af6cf1c3b7b2181))
* **security:** guard_git_url rejects file:// and other non-remote schemes ([363dec7](https://github.com/Kehl-io/nestweaver/commit/363dec77fbd01d1536b1d149f6193a5bf980441b))
* **security:** harden auth, admin gates, MCP tool access, watch path validation ([02fc18d](https://github.com/Kehl-io/nestweaver/commit/02fc18d5d794809e213128f83cb37008ee144f40))
* **security:** isolate git from system/global config and credential helpers ([4e799c7](https://github.com/Kehl-io/nestweaver/commit/4e799c7835462c515eea65cbc87e3396f4dbd13d))
* **security:** require auth for /metrics on the network listener ([b17e4b5](https://github.com/Kehl-io/nestweaver/commit/b17e4b5bbe9c2879a77009e5ebc79a8e3328b930))
* **security:** separate git fetch refspec with -- to prevent flag injection ([15ac391](https://github.com/Kehl-io/nestweaver/commit/15ac3915fa37828d21b653fdb5d40a7d2eaf696e))
* **security:** single shared MUTATING_TOOLS const + non-loopback MCP auth assertion ([5edacf4](https://github.com/Kehl-io/nestweaver/commit/5edacf415a0cde10dae55f991788deaa51809e51))
* **security:** write upstream token file with 0600 permissions ([3ba69d5](https://github.com/Kehl-io/nestweaver/commit/3ba69d5226ca5417335a38b2070263bf62c757eb))
* **server:** close hybrid readiness gaps ([7a25fb7](https://github.com/Kehl-io/nestweaver/commit/7a25fb7378ecfa42b38c82b718c88c4c7e8ef38a))
* **server:** enforce auth, queue, response, and casing invariants at single chokepoints ([67a8dcf](https://github.com/Kehl-io/nestweaver/commit/67a8dcfe7987b61b4a913e187ca6e2c14458f4a5))
* **server:** flow trace provenance, workspace discovery, timeout cancellation, port overflow ([be7517b](https://github.com/Kehl-io/nestweaver/commit/be7517b152626f9b7b5ad0810705d41a3dc4090a))
* **server:** harden hybrid server readiness ([b127b20](https://github.com/Kehl-io/nestweaver/commit/b127b206b66ef8ab9bdfaa1815b890b978d60da0))
* **server:** read_symbols pins to indexed_sha; ship a consistent frontend bundle ([58b78c8](https://github.com/Kehl-io/nestweaver/commit/58b78c873c2fa467399cbecf99b5557e41956c07))
* **server:** stop add_repo corrupting configs + fail fast on malformed --config ([f267c60](https://github.com/Kehl-io/nestweaver/commit/f267c60031739743d81c9cdb916eaeed720c00aa))
* stable UDS hash, diff rename handling, SSRF CGNAT block ([0c9402c](https://github.com/Kehl-io/nestweaver/commit/0c9402c653382ffb8be8f8e6178ba90ae925d364))
* **store,mcp:** auto-recover stale WAL checkpoint (P1); brain_search limit:0 panic ([a5644d5](https://github.com/Kehl-io/nestweaver/commit/a5644d5242695b2dea4dace27ff37f7500377c8b))
* **store:** add canonical_id graph queries for cross-repo impact ([d15c7c0](https://github.com/Kehl-io/nestweaver/commit/d15c7c09c9e5aa5dcc0b0b883537de12d8bed8ad))
* **store:** callees_of follows IMPORTS edges for richer flow_trace ([0a1cc6f](https://github.com/Kehl-io/nestweaver/commit/0a1cc6f96626359ba3a957b44e29d2066b53d0e6))
* **store:** cancelled vector search returns Err, not empty Ok ([713f803](https://github.com/Kehl-io/nestweaver/commit/713f803f4e58342398664711c147526f51cd20c7))
* **store:** cap regex_search pattern length and compile size ([892e89f](https://github.com/Kehl-io/nestweaver/commit/892e89f4bbc9912b920ed35d69d27ef2235cbf80))
* **store:** don't recompile lbug's third_party libs when lbug builds from source ([b0914b4](https://github.com/Kehl-io/nestweaver/commit/b0914b45eeb69d2498168b236b3d0ed03717e51d))
* **store:** parameterize remaining Cypher queries in delete_vault_cascade ([a8426cf](https://github.com/Kehl-io/nestweaver/commit/a8426cfce58f9dab73a63f0fcc9b8eee89c1d2ca))
* **store:** reject dimension-mismatched embeddings to keep the sidecar valid ([e4c592a](https://github.com/Kehl-io/nestweaver/commit/e4c592a90f6436730246c7e3d314df612a093728))
* **store:** use MERGE for File nodes to prevent duplicate primary key on re-index ([9c2f52d](https://github.com/Kehl-io/nestweaver/commit/9c2f52d669ae589187e41595e1c292e8d34e435e))
* strip trailing slashes before .git in canonical_repo_id ([3b0d28a](https://github.com/Kehl-io/nestweaver/commit/3b0d28ad4dd1a83c5806d01d8b767e7ed1153343))
* support GitLab X-Gitlab-Token webhook authentication ([a87cccc](https://github.com/Kehl-io/nestweaver/commit/a87ccccfaacdac3acb30fb222965780b0488f50b))
* **test:** format_comment test uses correct ImpactReport JSON shape ([e1b3b8e](https://github.com/Kehl-io/nestweaver/commit/e1b3b8ef8ca4c0046f76250bbc46c85ce73ae810))
* **test:** line_shift test checks scoped symbols only, not top-level ([8514486](https://github.com/Kehl-io/nestweaver/commit/85144869e1d188d2d6db8f871484dbff2b667de6))
* **test:** update format_comment test to use correct command ([e2f68bb](https://github.com/Kehl-io/nestweaver/commit/e2f68bb45b0341d53f99170f1650562b5642f0a6))
* thread branch config through scheduler so polling respects per-repo branch ([fe4cdf7](https://github.com/Kehl-io/nestweaver/commit/fe4cdf7650f5f96eff32facb9116844ac33e1094))
* thread branch through job queue and worker so configured branches are indexed ([540cd52](https://github.com/Kehl-io/nestweaver/commit/540cd526557d5937009c22bbb968a3aa6cd386c1))
* track in-flight jobs so queue depth reflects running work ([f21e8fe](https://github.com/Kehl-io/nestweaver/commit/f21e8fe51e6d8c33f2ef9eb4b500c7020c82246c))
* use per-upstream configured timeout in server-preferred and fallback paths ([66306f9](https://github.com/Kehl-io/nestweaver/commit/66306f9c7a7092f4e25f35b8141c51e3be4ebd37))
* **vault:** atomic delete+insert reindex so readers never see an empty vault ([d7162c4](https://github.com/Kehl-io/nestweaver/commit/d7162c4264f23e7b8639462606fa0d5a9b0f4c9d))
* warn on ignored server flags, log bare-clone file skips, clean up dead code ([738621d](https://github.com/Kehl-io/nestweaver/commit/738621d3cb0ecacc38afea78fc1abe71a47867f0))
* **web,security:** drop permissive CORS from the loopback UI router ([0681558](https://github.com/Kehl-io/nestweaver/commit/068155814ba97d1c04556f1fbabe2f285a08cd93))
* **web,security:** stored XSS in exports, source panic, unbounded graph depth ([69e3757](https://github.com/Kehl-io/nestweaver/commit/69e37571d8d370e2daa583528e9128b346a59f4f))
* **web:** bound the device-flow endpoints and validate config-sourced repos ([a8a74af](https://github.com/Kehl-io/nestweaver/commit/a8a74af6f44fe8ea42db3624442fab58d59ca71a))
* **web:** harden repo URL SSRF validation ([a1fd53e](https://github.com/Kehl-io/nestweaver/commit/a1fd53ef13a71f127dfd1691d5adc541c6f2841b))
* webhook and worker use same jobs.sqlite path ([64b86ae](https://github.com/Kehl-io/nestweaver/commit/64b86ae816ff9e3ebcd208bef427b63d849ef67a))
* webhooks skip repos with poll=manual or not in config ([5b43328](https://github.com/Kehl-io/nestweaver/commit/5b433280b459dab700422da614db0c1d730c9d49))
* **web:** surface live gRPC/MCP connection counts to admin dashboard ([3b43bc9](https://github.com/Kehl-io/nestweaver/commit/3b43bc984dc9513e11c5a0dd96665fdd4a1dc584))
* wire HybridClient into MCP-over-HTTP query dispatch ([3ad1c6d](https://github.com/Kehl-io/nestweaver/commit/3ad1c6de7d52a982e454de28e7ff655289772a9d))
* wire scheduler command channel so /admin/api/reload updates live poll state ([6504ee4](https://github.com/Kehl-io/nestweaver/commit/6504ee46528a6f1a2cb0331aa7d5c78ac6ecc7f8))
* worker pool acquires daemon write_mutex before indexing ([2c26911](https://github.com/Kehl-io/nestweaver/commit/2c269118a3801412b2c335d32123a6642f6db146))
* worker skips cancelled jobs, queue UI fields, token env expansion, docs ([6b9ff66](https://github.com/Kehl-io/nestweaver/commit/6b9ff660406eac29e3c2fbb32d63750c1459eacf))
* **worker:** circuit-open rejections don't burn the retry budget ([3800eed](https://github.com/Kehl-io/nestweaver/commit/3800eed27a02623ec3e9ea968e6ac00ee08164d3))
* **worker:** drop ungated pre-delete; bulk_reindex_write owns atomic swap ([8f59338](https://github.com/Kehl-io/nestweaver/commit/8f593385e5214b341c51638590b84725511f3685))


### Performance Improvements

* **engine:** pool git cat-file --batch in GitBareReader ([c7aa72d](https://github.com/Kehl-io/nestweaver/commit/c7aa72d1cc724cc2c4b5a6144e3b47dcb0196b11))

## [2.1.1](https://github.com/Kehl-io/nestweaver/compare/v2.1.0...v2.1.1) (2026-06-25)


### Bug Fixes

* **daemon:** remove dual-Database pattern — use single store for reads and writes ([3fbc4f9](https://github.com/Kehl-io/nestweaver/commit/3fbc4f9c7f8e7657d94d4acd1bf12d8f0190841b))
* **daemon:** serialize write RPCs with tokio Mutex ([4320814](https://github.com/Kehl-io/nestweaver/commit/4320814b1fdc840641c867a905319ad2802dd395))
* **engine:** move SHA update to after bulk_index_write in full-index path ([7fe88e4](https://github.com/Kehl-io/nestweaver/commit/7fe88e4acfec26ca5bd27828ba95cacb5664fd0e))
* ensure frontend assets are embedded in release binaries ([2b5e371](https://github.com/Kehl-io/nestweaver/commit/2b5e371f4bbb342e929d703a397c7dfcc91cc81f))
* ensure frontend assets are embedded in release binaries ([2e46a09](https://github.com/Kehl-io/nestweaver/commit/2e46a091858888446acf07faa85f557ac48a96ad))
* repo SHA stale reads — single store, write serialization, atomic SHA update ([1a110af](https://github.com/Kehl-io/nestweaver/commit/1a110af466f48cfde45026a22530137ba0e75e0d))
* **store:** wrap update_repo_sha DELETE+CREATE in explicit transaction ([735cc98](https://github.com/Kehl-io/nestweaver/commit/735cc9857bf00a993298536426b7bcdb4e97e637))

## [2.1.0](https://github.com/Kehl-io/nestweaver/compare/v2.0.1...v2.1.0) (2026-06-25)


### Features

* add competitive benchmark suite ([4953491](https://github.com/Kehl-io/nestweaver/commit/49534913825f10948a8d8d4ecd45d59d503b2476))
* **client:** graceful restart via Shutdown RPC ([c6f4b71](https://github.com/Kehl-io/nestweaver/commit/c6f4b712143e6dfd4120ec7c8e1d871b6d3ff349))
* **daemon:** graceful drain shutdown with configurable ceiling ([e51da65](https://github.com/Kehl-io/nestweaver/commit/e51da65184af0355cf290a9dac400c06305da3f5))


### Bug Fixes

* **benchmarks:** disable errexit in competitor functions to prevent silent crashes ([dbfa7d7](https://github.com/Kehl-io/nestweaver/commit/dbfa7d7ad499dc633b4be295d18d32d92d5db32b))
* **cli:** close sole-writer enforcement gaps ([b64da32](https://github.com/Kehl-io/nestweaver/commit/b64da32dae7de88daaed305fcc998a37de810a4d))
* remove leftover db_opened_at reference, fix clippy and fmt ([e7c55a1](https://github.com/Kehl-io/nestweaver/commit/e7c55a1b7fc44f871eeea958d7b167d35c0a724a))

## [2.0.1](https://github.com/Kehl-io/nestweaver/compare/v2.0.0...v2.0.1) (2026-06-24)


### Bug Fixes

* **daemon:** add refresh_db_opened_at to embed RPC ([c750f66](https://github.com/Kehl-io/nestweaver/commit/c750f66bbfa1bc0900c304b02f402f5d6ee58e57))
* **daemon:** change db_opened_at to AtomicU64 ([fd6f70d](https://github.com/Kehl-io/nestweaver/commit/fd6f70d007090bb9f12e13e68c1125492d706e04))
* **daemon:** refresh db_opened_at after write RPCs ([7f72dcf](https://github.com/Kehl-io/nestweaver/commit/7f72dcf756af9f5835ea747946104152b0c37e81))
* snapshot stamp.json bugs + materialize broken-pipe + release checksums ([dcd8ed2](https://github.com/Kehl-io/nestweaver/commit/dcd8ed2aefa86f724078558c283195be5200af75))
* **snapshot:** read embedding_model_id from [embedding] config ([0e88b1c](https://github.com/Kehl-io/nestweaver/commit/0e88b1c52a9d4cec92b14d57ae3e2ed3d117bb20))
* **snapshot:** use daemon-style instance_id for repo filtering ([9bd1123](https://github.com/Kehl-io/nestweaver/commit/9bd1123dec85338978cf60ca1eaa63347bc2b9a6))

## [2.0.0](https://github.com/Kehl-io/nestweaver/compare/v1.1.3...v2.0.0) (2026-06-23)


### ⚠ BREAKING CHANGES

* **app:** The macOS app bundle is now the recommended install method on Mac. The app provides a menubar status icon, Metal GPU acceleration for embeddings (~5x faster), automatic daemon lifecycle with crash recovery, daemon coexistence detection, and a managed web UI on port 9377. Favicon and app icon switched from dark to light variant (transparent background).

### Features

* **app:** add daemon coexistence, web UI lifecycle, and icon polish ([#98](https://github.com/Kehl-io/nestweaver/issues/98)) ([ffc95f1](https://github.com/Kehl-io/nestweaver/commit/ffc95f18a1446f4fc9b9d5ea89a8d11c2f3af561))
* **app:** native macOS app with menubar, Metal GPU, and managed daemon ([#100](https://github.com/Kehl-io/nestweaver/issues/100)) ([38cbbef](https://github.com/Kehl-io/nestweaver/commit/38cbbef666558262842846e3cd2cc76d2ca6f4e5))


### Bug Fixes

* **app:** use template image for menubar icon with transparent background ([7e30ce7](https://github.com/Kehl-io/nestweaver/commit/7e30ce7540a5088045d9c70525043494214bc990))

## [1.1.3](https://github.com/Kehl-io/nestweaver/compare/v1.1.2...v1.1.3) (2026-06-22)


### Bug Fixes

* **daemon:** detect stale daemon via DB mtime and auto-restart on connect ([a01badd](https://github.com/Kehl-io/nestweaver/commit/a01badd1ede54437df109ffe9b524d7cca31f235))
* **index:** resolve git HEAD SHA instead of hardcoding 'local' ([37a06dd](https://github.com/Kehl-io/nestweaver/commit/37a06dd65ded6bae4aafc4884b4355bfa4861dba))
* **mcp:** propagate store errors in brain_status instead of returning empty ([b698edb](https://github.com/Kehl-io/nestweaver/commit/b698edb5488e54f79e6309185f04f81b43c6fe76))

## [1.1.2](https://github.com/Kehl-io/nestweaver/compare/v1.1.1...v1.1.2) (2026-06-22)


### Performance Improvements

* indexing performance, daemon routing, and agent-friendly setup ([#94](https://github.com/Kehl-io/nestweaver/issues/94)) ([1f75479](https://github.com/Kehl-io/nestweaver/commit/1f7547904a66236969bd539fe9da52be5c269a4e))

## [1.1.1](https://github.com/Kehl-io/nestweaver/compare/v1.1.0...v1.1.1) (2026-06-21)


### Bug Fixes

* normalize repo URLs and update docs for v1.1 ([#91](https://github.com/Kehl-io/nestweaver/issues/91)) ([4e68c38](https://github.com/Kehl-io/nestweaver/commit/4e68c382496156c71ef77cf30055d31d3025b8d0))

## [1.1.0](https://github.com/Kehl-io/nestweaver/compare/v1.0.1...v1.1.0) (2026-06-20)


### Features

* update dependencies, launchd daemon, and macOS app bundle ([#88](https://github.com/Kehl-io/nestweaver/issues/88)) ([f6c299b](https://github.com/Kehl-io/nestweaver/commit/f6c299b7aec291668af2eb75204c1a3ede90631f))

## [1.0.1](https://github.com/Kehl-io/nestweaver/compare/v1.0.0...v1.0.1) (2026-06-20)


### Bug Fixes

* **embed:** use rustls instead of native-tls to fix cross-compilation ([1092704](https://github.com/Kehl-io/nestweaver/commit/1092704b0f0ef26b133a23f170236c673247f7fc))

## [1.0.0](https://github.com/Kehl-io/nestweaver/compare/v0.28.0...v1.0.0) (2026-06-20)


### ⚠ BREAKING CHANGES

* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82))

### Features

* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82)) ([eceb68d](https://github.com/Kehl-io/nestweaver/commit/eceb68da3f685d4e1d71a8e900397506a073beab))

## [0.28.0](https://github.com/Kehl-io/nestweaver/compare/v0.27.0...v0.28.0) (2026-06-18)


### Features

* **daemon:** eliminate all stop_daemon_if_running calls ([ec6d2c2](https://github.com/Kehl-io/nestweaver/commit/ec6d2c27ff5e6d50e25099f45aacb4528bb9fa04))
* enrich brain_guide/admin instructions with tool-routing table, add staleness warnings and token hints ([f5db6f7](https://github.com/Kehl-io/nestweaver/commit/f5db6f7d6e4590fa9df9869c26c98c5fd6c8d026))


### Bug Fixes

* bump gRPC message size limit from 64MB to 256MB (fixes export cypher) ([6306a48](https://github.com/Kehl-io/nestweaver/commit/6306a48a96224cf2398cbbdb090d7b81a3010d5d))
* **ci:** set NESTWEAVER_NO_DAEMON=1 for E2E tests ([2cb77a4](https://github.com/Kehl-io/nestweaver/commit/2cb77a497ca005864e1881232c7fa6aa2c011ee1))
* **cli:** add daemon RPCs for list-repos/services/projects/search/symbol ([55f2b50](https://github.com/Kehl-io/nestweaver/commit/55f2b507f966ac435aff73a7f9d7e42c6c7761a0))
* **cli:** graceful daemon fallback for ui/watch, fix collapsible-if clippy warnings ([9790a5e](https://github.com/Kehl-io/nestweaver/commit/9790a5eb62336717021e1276eaef3692c3757e2e))
* **cli:** guard --no-daemon for testing, route brain-remove/snapshot through daemon ([6450281](https://github.com/Kehl-io/nestweaver/commit/645028128aa4267ac0821462d3c81778a9684791))
* **cli:** guard --no-daemon in MCP server command consistently ([32f8472](https://github.com/Kehl-io/nestweaver/commit/32f8472fd80ee719eaafdcfc9fb87777aa150c0a))
* **cli:** route 10 read commands through daemon, improve lock error message ([5486618](https://github.com/Kehl-io/nestweaver/commit/5486618d838838be03fc0babd4bbcebfa34952de)), closes [#63](https://github.com/Kehl-io/nestweaver/issues/63)
* **cli:** route all 5 json-gated commands through daemon unconditionally ([8cb5152](https://github.com/Kehl-io/nestweaver/commit/8cb5152609cd0d907545031f9c8862814190ae86))
* **cli:** route context/cross-repo-refs/generate-guide/cluster through daemon ([7f8377a](https://github.com/Kehl-io/nestweaver/commit/7f8377af3c55811a1fb047406437f9c99e8dfede))
* **cli:** route export and ui commands through daemon stop/restart ([9063194](https://github.com/Kehl-io/nestweaver/commit/9063194cda6f38e65c96ad9f689edefd48e6b0bf))
* **cli:** route remove-repo/remove-project resolution through daemon RPCs ([800f549](https://github.com/Kehl-io/nestweaver/commit/800f549be3cffcd5434de2fa5e371c5559fcdc0e))
* **cli:** route repo-map/suggest-links/detect-implicit-projects/pr-impact through daemon ([8a57a4c](https://github.com/Kehl-io/nestweaver/commit/8a57a4c84f0cf29e0e9f99777e437794401ccafd))
* **cli:** route watch/export/ui through daemon (zero bypasses remaining) ([4bda18b](https://github.com/Kehl-io/nestweaver/commit/4bda18bc0ca3da9afaab93461cbe51a83e50d9a5))
* route CLI read commands through daemon (fixes [#63](https://github.com/Kehl-io/nestweaver/issues/63)) ([d71c53f](https://github.com/Kehl-io/nestweaver/commit/d71c53f7abeec61e743c342ee0b69530afc0bf72))
* **web:** commit AppState Arc&lt;TantivyIndex&gt; refactor (fixes CI build) ([6274f31](https://github.com/Kehl-io/nestweaver/commit/6274f31ef70653037de4e19d342823b9014ed888))

## [0.27.0](https://github.com/Kehl-io/nestweaver/compare/v0.26.3...v0.27.0) (2026-06-17)


### Features

* add remove-repo CLI command and daemon RPC ([f9f0bac](https://github.com/Kehl-io/nestweaver/commit/f9f0bac5b1771f3438fa85df3fa5e78eac519e43))
* **cli:** add remove-project and prune-stale commands ([cc7682a](https://github.com/Kehl-io/nestweaver/commit/cc7682abaca500bb74e20c88bdf9981352fa27eb))
* **cli:** add remove-repo command ([f0ad081](https://github.com/Kehl-io/nestweaver/commit/f0ad081aeaa6c91f1a05b8c824347ca96d35e19c))
* **client:** add remove_project and prune_stale methods ([0555361](https://github.com/Kehl-io/nestweaver/commit/05553616a6155879b585e4141da7902ad1ff61de))
* **client:** add remove_repo method ([1e1d8db](https://github.com/Kehl-io/nestweaver/commit/1e1d8dba04d24fbe54756764a8a856d5e29ad368))
* **daemon:** implement RemoveProject and PruneStale handlers ([7723612](https://github.com/Kehl-io/nestweaver/commit/772361257d648cdd7e9de0e757aa3017022761d4))
* **daemon:** implement RemoveRepo RPC handler ([29c3fdc](https://github.com/Kehl-io/nestweaver/commit/29c3fdcfd81b53cfcd7a636d31ae97c2e3cd0c45))
* **mcp:** add brain_remove_source and prune_stale tools ([f46ca49](https://github.com/Kehl-io/nestweaver/commit/f46ca49e6b2797da22913392a80d3662abfb249e))
* **proto:** add RemoveProject and PruneStale RPCs ([71a4126](https://github.com/Kehl-io/nestweaver/commit/71a412659a8bfc8555db342bd07164580608a3ce))
* **proto:** add RemoveRepo RPC message and service method ([5f72262](https://github.com/Kehl-io/nestweaver/commit/5f72262c1d0ad213f3e3bf054d91192ea1a7d0cd))


### Bug Fixes

* clippy/fmt fixes + docs for remove-project, prune-stale, brain_remove_source ([020254b](https://github.com/Kehl-io/nestweaver/commit/020254b0a7235aa473eb0c38489adaa62a1c7164))

## [0.26.3](https://github.com/Kehl-io/nestweaver/compare/v0.26.2...v0.26.3) (2026-06-17)


### Bug Fixes

* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([6813314](https://github.com/Kehl-io/nestweaver/commit/6813314fa8f7eef6f722754eba8a5ae1f72b5032))
* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([3a1edf6](https://github.com/Kehl-io/nestweaver/commit/3a1edf6476b23813559f5e288d116adf866ede90))

## [0.26.2](https://github.com/Kehl-io/nestweaver/compare/v0.26.1...v0.26.2) (2026-06-17)


### Bug Fixes

* **daemon:** derive socket path from stable per-user dir, not $TMPDIR ([#74](https://github.com/Kehl-io/nestweaver/issues/74)) ([2febd6e](https://github.com/Kehl-io/nestweaver/commit/2febd6ee1095b4c04f127d4aaa944cb6b2cf8409))

## [0.26.1](https://github.com/Kehl-io/nestweaver/compare/v0.26.0...v0.26.1) (2026-06-17)


### Bug Fixes

* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([6971396](https://github.com/Kehl-io/nestweaver/commit/6971396a31fac3e8e81df16e9f5821173ee5c338))
* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([39b5ba5](https://github.com/Kehl-io/nestweaver/commit/39b5ba5892784b14df025231e259e1731b11334e))

## [0.26.0](https://github.com/Kehl-io/nestweaver/compare/v0.25.1...v0.26.0) (2026-06-16)


### Features

* **cli:** wire [limits].default_result_limit to CLI search and context commands ([d804176](https://github.com/Kehl-io/nestweaver/commit/d804176a9611fbdeae9610d8da0006a616f510a9))
* **config:** wire [limits].default_result_limit to runtime tool dispatch ([814c4bd](https://github.com/Kehl-io/nestweaver/commit/814c4bdd9016a9939bda0990bb01039e9eafe1b2))
* wire [limits].default_result_limit to runtime dispatch ([40a5c80](https://github.com/Kehl-io/nestweaver/commit/40a5c80ba1f3cc6c1a42dd568605bb133e456b75))


### Bug Fixes

* **mcp:** log instance config discovery, add automated tests for wired limits ([37e49da](https://github.com/Kehl-io/nestweaver/commit/37e49da5b6fc7947df73aa1c67f4351ea2d53c77))

## [0.25.1](https://github.com/Kehl-io/nestweaver/compare/v0.25.0...v0.25.1) (2026-06-16)


### Bug Fixes

* address code review findings for production readiness ([4ac2627](https://github.com/Kehl-io/nestweaver/commit/4ac262714641557675bc80e24802bbcb698ffd86))
* **daemon:** serialize env-mutating lifecycle tests to prevent flaky failures ([ed3eed3](https://github.com/Kehl-io/nestweaver/commit/ed3eed3e8e2b2964b502fa6004dc693f1b1025d4))
* **engine:** correct contract_drift boundary check, use store lookup for tag titles ([d1f034d](https://github.com/Kehl-io/nestweaver/commit/d1f034d28fb1d253706558d0f15ef07be9a9d9c5))
* **engine:** prevent double-concatenation of Spring/NestJS route prefixes in contract_drift ([c8110c8](https://github.com/Kehl-io/nestweaver/commit/c8110c8d873dc409113223819c7437ec6ddd4e5f))
* **engine:** resolve tag titles in brain_context instead of showing raw UIDs ([ac4c3a6](https://github.com/Kehl-io/nestweaver/commit/ac4c3a6cbd1702cadad08cdaed3a18fe88e40364))
* **mcp:** brain_search concise mode, stale_check commits behind, pagination limits ([b6db270](https://github.com/Kehl-io/nestweaver/commit/b6db270d50b7b86882eea90e4dd1b1ddba74d68f))
* **mcp:** centralize limit default, fix inner-class flow_trace in tools ([27b53ff](https://github.com/Kehl-io/nestweaver/commit/27b53ff192d7acb0cac6fd545daf8f71f138c3c8))
* **mcp:** collapse nested if-let blocks to satisfy clippy collapsible_if ([6cbcb75](https://github.com/Kehl-io/nestweaver/commit/6cbcb75476eaf377a87c66a847745faf6f5fa982))
* **mcp:** compute staleness_commits_behind in stale_check, add limit to broken_links and orphan_documents ([5bab151](https://github.com/Kehl-io/nestweaver/commit/5bab15128d201a9dde68acf7a32b130280e57ffb))
* **mcp:** fix flow_trace class expansion, add pagination to 8 tools, slug-tolerant note_get ([d796686](https://github.com/Kehl-io/nestweaver/commit/d79668623fefbc8326042aa60df231d5f2cb7938))
* **mcp:** respect concise mode in daemon search proxy, add note location, fix tag title fallback ([98ce6cc](https://github.com/Kehl-io/nestweaver/commit/98ce6cc5165f74afa0a1b75fc1808524be8a71db))
* **mcp:** slug-tolerant backlinks lookup, hub_nodes clustering hint, regex_search truncated clarity ([9b57f66](https://github.com/Kehl-io/nestweaver/commit/9b57f6642604b74ce8b7c12ded908c2b1ce0ddd8))
* **proto:** mark semantically-optional proto3 fields as optional ([e2b59a9](https://github.com/Kehl-io/nestweaver/commit/e2b59a9aa71116492f417b7cc1851d7c33384c6d))
* **store:** add members_of() query and fix inner-class flow_trace scoping ([f1d0f5c](https://github.com/Kehl-io/nestweaver/commit/f1d0f5c812f4ee647b6fb0d3f3cdae7dafed1a3a))


### Performance Improvements

* **store:** add lookup_tag() for O(1) tag resolution, replace list_tags() scans ([1b9f15b](https://github.com/Kehl-io/nestweaver/commit/1b9f15b0bffee59ee25cc820875a8a011c962fe0))

## [0.25.0](https://github.com/Kehl-io/nestweaver/compare/v0.24.0...v0.25.0) (2026-06-16)


### Features

* **client:** add materialize_projects, remove_vault, merge_instance, purge_instance methods ([952cd35](https://github.com/Kehl-io/nestweaver/commit/952cd35cd50546b43ee3f1df8c3b05946df72dfb))
* **daemon:** implement MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance handlers ([3d17b1b](https://github.com/Kehl-io/nestweaver/commit/3d17b1b6b4ff5267e16ee3139e37eceff7293914))
* **proto:** add MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance RPCs ([9c9e10d](https://github.com/Kehl-io/nestweaver/commit/9c9e10de2bac7b39f13a49a56a21298166f3cabe))
* route all database writes through daemon RPC ([ed2396d](https://github.com/Kehl-io/nestweaver/commit/ed2396d6ee54caa2013f8d698c095834fedccfad))


### Bug Fixes

* **cli:** keep direct-write fallback for index/brain-add when NESTWEAVER_NO_DAEMON=1 ([aa0ae50](https://github.com/Kehl-io/nestweaver/commit/aa0ae5084ffe3ace0aede3c6a74ea6f52606fc8c))
* **daemon:** decrement active_connections on early return in materialize_projects, restore dist ([575eae8](https://github.com/Kehl-io/nestweaver/commit/575eae888ff71264793e19b7fed45657b00e79ff))
* **daemon:** wrap unary handlers in spawn_blocking, add active_connections tracking, include discarded notes count ([6cf7298](https://github.com/Kehl-io/nestweaver/commit/6cf72981775fa628192344ac32653e492e1d9096))
* **ui:** open store read-write for watch mode, read-only for default UI ([5fc082d](https://github.com/Kehl-io/nestweaver/commit/5fc082d5a2412942ca18cc5da573d591fc445135))

## [0.24.0](https://github.com/Kehl-io/nestweaver/compare/v0.23.0...v0.24.0) (2026-06-16)


### Features

* **graph:** add node preview card component ([a6ea76f](https://github.com/Kehl-io/nestweaver/commit/a6ea76f61084dfbb25b0f2865d90d368bed86939))
* **graph:** add preview card state to store ([c67e851](https://github.com/Kehl-io/nestweaver/commit/c67e85144e557056fd2ecb733c13901c44392338))
* **graph:** add useNodePreview hook with LRU cache ([66b378e](https://github.com/Kehl-io/nestweaver/commit/66b378e5aac41559fe5a4043e8aa057041413ed3))
* **graph:** Obsidian-style visual reskin + click-to-preview UX ([e33eaf5](https://github.com/Kehl-io/nestweaver/commit/e33eaf5a2de5b370e36a62dafe0d27aa1f27c4f0))
* **graph:** populate context menu with grouped power actions ([5ff9e7a](https://github.com/Kehl-io/nestweaver/commit/5ff9e7a1295754742dfc79bcfc0eccf545392a5f))
* **graph:** wire click-to-preview and escape dismiss ([cd34cea](https://github.com/Kehl-io/nestweaver/commit/cd34cea1968b580c8bc70d19de0aeb4359c33f37))


### Bug Fixes

* **e2e:** don't toggle settings between view switches — flyout stays open ([3ac9efb](https://github.com/Kehl-io/nestweaver/commit/3ac9efb0e7fd77d0aaf6f5f55c95efefb761e6f2))
* **e2e:** fix settings flyout interactions — close after toggle, remove stale Export/Filter button refs ([0eef514](https://github.com/Kehl-io/nestweaver/commit/0eef5141f06f3550a398e376b1bb1ab3e191d21c))
* **e2e:** force-click view buttons in settings flyout to bypass visibility check ([966e3f9](https://github.com/Kehl-io/nestweaver/commit/966e3f95eae9eacbe232cc8281af2e5815bf182c))
* **e2e:** wait for settings flyout visibility before clicking view buttons ([7946460](https://github.com/Kehl-io/nestweaver/commit/7946460b790b26725c47dbd1847399ea66efd3ae))
* **graph:** render ExportMenu inline inside settings flyout to prevent overlay ([50f3ce5](https://github.com/Kehl-io/nestweaver/commit/50f3ce5c701c35e1d498dd63aa5e1420fc96d344))
* **graph:** review fixes — open-source-file path, context menu dismiss, button types ([c39effe](https://github.com/Kehl-io/nestweaver/commit/c39effe6b225d51a48449b06514c24bef9715a65))
* **graph:** show fallback node info when API detail unavailable ([4f08f9f](https://github.com/Kehl-io/nestweaver/commit/4f08f9fae1267e9c9ccb05af0d264789d05d0fc5))
* **graph:** wire right-click context menu via store, fix Escape handler conflict ([0d57c38](https://github.com/Kehl-io/nestweaver/commit/0d57c38bb2126bc8bae0632a7d8f72303975960f))
* reduce baseScale to 0.45 to compensate for larger hard-edge circle ([2328926](https://github.com/Kehl-io/nestweaver/commit/23289260997203583b37b07aeed320683a971578))
* restore base scale factor (0.62) for node sizing ([2bcf49c](https://github.com/Kehl-io/nestweaver/commit/2bcf49c2d8710c63f7e62d4a4b88b571ffc7b246))


### Performance Improvements

* remove bloom post-processing, update background to Catppuccin base ([d5a8525](https://github.com/Kehl-io/nestweaver/commit/d5a8525876f9f32db4062343c1a92560393d825c))

## [0.23.0](https://github.com/Kehl-io/nestweaver/compare/v0.22.0...v0.23.0) (2026-06-13)


### Features

* **cli:** accept --track-interactions on daemon start with redirect message ([7e99589](https://github.com/Kehl-io/nestweaver/commit/7e995892247c12b48afd11bd230804d6d35401ce))
* **cli:** add daemon routing to 10 high-frequency CLI commands ([edc58e5](https://github.com/Kehl-io/nestweaver/commit/edc58e55065f2bbc888aea5551bb04fcb3e806e9))
* **daemon:** extend try_daemon_json_rpc with 19 additional RPC method routes ([3c32110](https://github.com/Kehl-io/nestweaver/commit/3c32110c346d15cec1c7ec7af17461ad24844a73))
* **interactions:** lower flush threshold to 5 and add time-based auto-flush ([31a0bd7](https://github.com/Kehl-io/nestweaver/commit/31a0bd7b1c6fd5e2e838958e87ff734c02e4ea67))
* **mcp:** expand interaction tracking to cover more tools ([908ea8e](https://github.com/Kehl-io/nestweaver/commit/908ea8e4e9cfd505c7f7349f147395b2cdd5ac54))
* **project:** wire tags, parent, and features from ProjectConfig ([c3e059c](https://github.com/Kehl-io/nestweaver/commit/c3e059c584d8616f29f0b556df290041a69bbf80))
* **proto:** extend BrainContextRequest for daemon parity ([6578e58](https://github.com/Kehl-io/nestweaver/commit/6578e585c51ac020a67aab5850c840db00aed178))
* **schema:** add ALL_SYMBOL_EDGE_TYPES constant and from_rel_table_name lookup ([57d0cc3](https://github.com/Kehl-io/nestweaver/commit/57d0cc332d90cc8ddf97df38b25bae59ff02cf12))


### Bug Fixes

* **cli:** rename UnlinkedVault to DiscardedVault, clarify merge output ([970bf13](https://github.com/Kehl-io/nestweaver/commit/970bf1316d46fd08aa209ff99cb0989abb7e577b))
* **store:** preserve SECTION_TAGGED_WITH edges during vault reparent ([859b245](https://github.com/Kehl-io/nestweaver/commit/859b24524c678611828fcd1365b906108f2e5008))
* **store:** prevent data loss in merge_instance_ids via reparent_vault ([0fb6531](https://github.com/Kehl-io/nestweaver/commit/0fb65314261edfe9e379c28af9b08e0afe5e8993))
* **v0.22.0:** instance merge dataloss, interaction tracking, daemon parity, edge-type centralization ([6738965](https://github.com/Kehl-io/nestweaver/commit/67389652e22acb25e4f3d2fdd665f5e7bcbb0e97))

## [0.22.0](https://github.com/Kehl-io/nestweaver/compare/v0.21.0...v0.22.0) (2026-06-13)


### Features

* **brain:** wave-3 — diagnostics polish + brain context filters ([c2caeb6](https://github.com/Kehl-io/nestweaver/commit/c2caeb6137f92dff05c905f05131effd4c3cb9de))
* **instance-remove:** add --purge-graph to cascade-delete instance data ([269635f](https://github.com/Kehl-io/nestweaver/commit/269635f8b535eca630deb1e013a766aafc3bbea3))
* **proto:** forward since/recency_weight through ProjectContextRequest ([979ef1e](https://github.com/Kehl-io/nestweaver/commit/979ef1eca5ebac329958a9592875b255c5196d25))
* **seed-resolution:** graduated path-deboost + kind-priority for symbol lookup ([bbfcb22](https://github.com/Kehl-io/nestweaver/commit/bbfcb226e871409ab434f6efc1e3319f267ee36f))


### Bug Fixes

* **brain-remove:** match stored vault paths regardless of canonical form ([f813959](https://github.com/Kehl-io/nestweaver/commit/f8139590cf6570f58f327d4c89fa4e4daaf9d24e))
* **brain-status:** expose instance_id per row + forward collision warnings through daemon ([0a94cbd](https://github.com/Kehl-io/nestweaver/commit/0a94cbd3b9463e524f3d8cfb4d1c40e98caab293))
* **brain:** close four v0.21.0 regressions surfaced by live-DB audit ([1629156](https://github.com/Kehl-io/nestweaver/commit/16291562ec9f9a0cfb4450e909f7969e01ed435d))
* **engine,cli:** three minor gaps from v0.21.0 adoption validation ([85f44e1](https://github.com/Kehl-io/nestweaver/commit/85f44e124a3e8906b0df065d436129bc8af920bb))
* **engine:** RepoConfig.name resolves repo aliases for projects and features ([dee57d9](https://github.com/Kehl-io/nestweaver/commit/dee57d9cfc9f2834340651d5309da26a8eeb71fc))
* **mcp:** route brain_status through JSON pass-through RPC ([889fa06](https://github.com/Kehl-io/nestweaver/commit/889fa06a17ecc432ee5b8b5b5739328d3d771bb2))
* **project-context:** surface member symbols, not just notes ([cd753e7](https://github.com/Kehl-io/nestweaver/commit/cd753e7bf25112cfabf59571386ecb8b1ebf6188))
* **purge-instance:** sweep orphan symbol/file/note rows by UID prefix ([c8c9cdb](https://github.com/Kehl-io/nestweaver/commit/c8c9cdb31ee6c96557bdb92eea3ccfd1c93a0e09))
* **v0.21.0:** regression fixes, adoption validation, and performance ([326845e](https://github.com/Kehl-io/nestweaver/commit/326845e16e3b08873402dcb4cbb559a7040ef413))


### Performance Improvements

* **cross-domain:** batch edge commits in single transactions ([1597555](https://github.com/Kehl-io/nestweaver/commit/15975552fcdb1e631233b324c960c2686c938b63))
* **engine,cli,mcp:** dedup Heading/Section pairs + daemon path for project-context ([bf95636](https://github.com/Kehl-io/nestweaver/commit/bf956360fce2232f0387bf102671aaa9a7821641))
* **store:** single-query orphan sweep and allocation-free kind_rank ([c35ec60](https://github.com/Kehl-io/nestweaver/commit/c35ec6052358346bd6004f3c9f337ef971d589ce))

## [0.21.0](https://github.com/Kehl-io/nestweaver/compare/v0.20.0...v0.21.0) (2026-06-10)


### Features

* **cli:** add --token-budget to nestweaver context ([91d84b6](https://github.com/Kehl-io/nestweaver/commit/91d84b61aa4099533ba7c116a062d6603996a8eb))
* improve graph UI readiness ([384a0c2](https://github.com/Kehl-io/nestweaver/commit/384a0c2316faf4e362b9106af54377822b96566a))
* **store:** config-driven test path deboost patterns ([7bdb2eb](https://github.com/Kehl-io/nestweaver/commit/7bdb2eb1543f8703b301822463c5ef6c5512c002))
* **web:** add contextual graph actions ([90c5e8d](https://github.com/Kehl-io/nestweaver/commit/90c5e8df54cb675d569188f676ed825425f37ea9))
* **web:** add dense graph views ([46ec6f9](https://github.com/Kehl-io/nestweaver/commit/46ec6f96408eaed56be6fe3969d9de6723a1f6b5))
* **web:** add overview API ([9d0814f](https://github.com/Kehl-io/nestweaver/commit/9d0814fc181f1f3d24d972b3d292976af1ed154e))
* **web:** add overview frontend types ([450d1ab](https://github.com/Kehl-io/nestweaver/commit/450d1aba3e7f12cb105cffee6c857c57aeff0999))
* **web:** add overview guidance surfaces ([06ac75b](https://github.com/Kehl-io/nestweaver/commit/06ac75bba8fb3c047d407eae9d874c5846a65dd6))
* **web:** bring graph nodes to life ([88f6e7f](https://github.com/Kehl-io/nestweaver/commit/88f6e7ffa78f2fe46effd54a16765c378fdbb729))
* **web:** group graph controls ([71b6fa2](https://github.com/Kehl-io/nestweaver/commit/71b6fa286a4bf8b2c3fae55f6d9ab33a70e3864b))
* **web:** load overview graph by default ([b9cbec1](https://github.com/Kehl-io/nestweaver/commit/b9cbec19bf1a85b7bf5e36c0a75acd36784505e9))


### Bug Fixes

* **brain:** remove ghost vault rows by path match, not just computed UID ([b5c162b](https://github.com/Kehl-io/nestweaver/commit/b5c162b9d441d7fbd27af97e4d3b15323b643c80))
* **cli:** resolve clippy warnings in daemon routing guards ([7c393e7](https://github.com/Kehl-io/nestweaver/commit/7c393e7324cae30fc694a7cf5f567e1eec2a6f44))
* **cli:** route brain context and 5 docgraph commands through daemon ([a56137f](https://github.com/Kehl-io/nestweaver/commit/a56137f476344c1b8a54ece5c250444ac58ea160))
* **cli:** route impact, regex-search, hubs, stale-check, status through daemon ([5378550](https://github.com/Kehl-io/nestweaver/commit/5378550b37cbec202174715ec1bbe1636e0e59c8))
* **daemon:** route `brain reindex-search` through daemon RPC ([ce70e67](https://github.com/Kehl-io/nestweaver/commit/ce70e678c13e475493743631ce182d669b577d16))
* **proto:** add instance_id to IndexVaultRequest, thread through daemon and MCP ([5b6855e](https://github.com/Kehl-io/nestweaver/commit/5b6855ed781d0ab6002b70fa8df68c06f20ffa02))
* resolve 6 reported bugs — lock warning, intent, interactions, merge, seed ranking, limit ([cdb97b2](https://github.com/Kehl-io/nestweaver/commit/cdb97b21f340d0ae2c8a73dd83bcfef17eaedf27))
* restore lock-error semantics in open_store, cascade-delete vault notes during merge ([2335fc2](https://github.com/Kehl-io/nestweaver/commit/2335fc25a9aded6c1d53eda435112cd77b5b05db))
* **store:** prefer populated vault on collision merge, warn about unlinked notes ([5d40d2d](https://github.com/Kehl-io/nestweaver/commit/5d40d2df592dcdf478aec5f2b98f98d7f3ed9351))
* **store:** safe vault UID rewrite during instance merge, add --limit docs ([5efa26b](https://github.com/Kehl-io/nestweaver/commit/5efa26b68eb512190e6b7adbcd5d798cb3a3b128))
* **web:** address review findings — brand colors, a11y, dedup, dead code ([025d003](https://github.com/Kehl-io/nestweaver/commit/025d003d0610f5e174247225ed835838e181f780))
* **web:** allow search dropdown to escape header bounds ([24e40a6](https://github.com/Kehl-io/nestweaver/commit/24e40a6004125e9d184031f2f9142f9065743bff))
* **web:** balance overview landmarks ([fbea9d6](https://github.com/Kehl-io/nestweaver/commit/fbea9d6760d14371e6c20585c2916f9d66960c2f))
* **web:** gate overview actions by supported targets ([f6e9aa8](https://github.com/Kehl-io/nestweaver/commit/f6e9aa8ac761e34e93983f0761f41104d630f48e))
* **web:** guard overview graph updates ([3ead808](https://github.com/Kehl-io/nestweaver/commit/3ead808dd4bcb81a71d01f570d1eea7f31ee4ece))
* **web:** make overview entry points actionable ([80694f6](https://github.com/Kehl-io/nestweaver/commit/80694f6b08ad80308e4c3373273d90eb3cd716f5))
* **web:** migrate persisted graph mode ([03782ac](https://github.com/Kehl-io/nestweaver/commit/03782ac38bdf5ea3abd5aaa9cabb509ec365a323))
* **web:** polish guided overview verification ([88887cf](https://github.com/Kehl-io/nestweaver/commit/88887cffb19ffa9900dddf910426888026a1f543))
* **web:** preserve overview landmark labels ([641a732](https://github.com/Kehl-io/nestweaver/commit/641a73237dd8e36aa20c74508155920baed267ef))
* **web:** preserve repo git suffix in overview ([3e2cd7f](https://github.com/Kehl-io/nestweaver/commit/3e2cd7fe4c16e942bc0b7529a653a1f9aaf23447))
* **web:** refine guided overview browser pass ([35f3e6a](https://github.com/Kehl-io/nestweaver/commit/35f3e6ab250a653edea4fd847e6b27934ef3bd73))
* **web:** reset graph mode on page load ([8ead92b](https://github.com/Kehl-io/nestweaver/commit/8ead92b158f2a95282f6f33fd288016a548e1fad))
* **web:** scope e2e search click to dropdown, not graph label ([9ab17e4](https://github.com/Kehl-io/nestweaver/commit/9ab17e48e2cd5c9a814bdaad60ba763c911e6de5))
* **web:** sharpen graph nodes and labels ([106a6ef](https://github.com/Kehl-io/nestweaver/commit/106a6ef18e04828b7917100ed580c99402db1540))
* **web:** simplify graph nodes to led dots ([56bd04e](https://github.com/Kehl-io/nestweaver/commit/56bd04e7fba864d358c5f0cf212821d14bf40fc5))
* **web:** soften graph node borders ([6ab3b00](https://github.com/Kehl-io/nestweaver/commit/6ab3b00e7848ba598e862ea8f4239aa1422d1d19))

## [0.20.0](https://github.com/Kehl-io/nestweaver/compare/v0.19.0...v0.20.0) (2026-06-08)


### Features

* **brain:** support cross-vault wikilinks with vault:note prefix ([5e28d75](https://github.com/Kehl-io/nestweaver/commit/5e28d758dcf820ea6b5c772ac6daf3e22e7c0946))
* **guide:** add CLAUDE.md generation format ([3dde889](https://github.com/Kehl-io/nestweaver/commit/3dde889b154a5c0570f157c941373e4304f4e697))
* markdown knowledge graph enhancements — CLAUDE.md gen, AgentConfig, canvas/dataview/mermaid parsers ([857cbbb](https://github.com/Kehl-io/nestweaver/commit/857cbbb1fa4147b019331a73d22b5d391fdc2d1f))
* **parser:** add Dataview DQL query parser ([5408a9f](https://github.com/Kehl-io/nestweaver/commit/5408a9ff28cfdfbdf0326162256e2f7075c5b126))
* **parser:** add Mermaid flowchart/graph diagram parser ([4aef510](https://github.com/Kehl-io/nestweaver/commit/4aef51056c4a2ef5a44765bf77fc53b25d544274))
* **parser:** add Obsidian canvas file parser ([e2918c9](https://github.com/Kehl-io/nestweaver/commit/e2918c91b86e068c2c6afe6f00b528172884eae7))
* **parser:** extract checkboxes and ADR sections from markdown ([2ff7e0b](https://github.com/Kehl-io/nestweaver/commit/2ff7e0bbb6375f71544ef587684934008dac53af))
* **schema:** add NoteKind::AgentConfig for agentic instruction files ([06e2780](https://github.com/Kehl-io/nestweaver/commit/06e27805cf1690e2170bcf446f1db07acc75291e))
* **setup:** add PreToolUse hook to intercept grep/find, fix token savings claims to validated 10x ([e3bd6dc](https://github.com/Kehl-io/nestweaver/commit/e3bd6dc1a45ebdf4fabb52e1f7a0d5057950b05d))
* **setup:** install Claude Code hooks, add token-savings messaging to skills and cursor rules ([fdd4867](https://github.com/Kehl-io/nestweaver/commit/fdd48673e2f89142de989f9b555ebf1e0e294619))


### Bug Fixes

* address code review findings — AgentConfig round-trip, drive letter guard, leaked configs, dead code, dataview tag regex ([dc27560](https://github.com/Kehl-io/nestweaver/commit/dc275608f007157c2e261d2d1680afd6a6c5f2bb))
* **cli:** stop daemon before direct-mode index to prevent lock contention ([3f65f63](https://github.com/Kehl-io/nestweaver/commit/3f65f63e811e089e1cd5c0a6d075e4983bd1734d))
* collapse nested if-let per clippy ([6760593](https://github.com/Kehl-io/nestweaver/commit/67605938fc57150ff81168e1d7ddb9d137861f11))
* **mcp:** log cache serialization failures instead of silently skipping ([acf771f](https://github.com/Kehl-io/nestweaver/commit/acf771f1712e0806399e535ec3d7cef040b437ad))
* rustfmt formatting for new parser modules ([19b4177](https://github.com/Kehl-io/nestweaver/commit/19b4177dd85bedde21c548bf2f2a6c26ee0eb1ba))
* **store:** address review findings — transaction safety, ordering, cache version ([9fb2ce4](https://github.com/Kehl-io/nestweaver/commit/9fb2ce4f30baebbd60ff7962dd0766b726801d55))
* **store:** hash vec lengths in scope_hash to prevent prefix collisions ([242390b](https://github.com/Kehl-io/nestweaver/commit/242390bd42b1cb3ae6fa628fe1029efcdb2cd60f))


### Performance Improvements

* comprehensive performance audit — 35x index, 10000x cache, parallel resolution ([efbff3b](https://github.com/Kehl-io/nestweaver/commit/efbff3b938f9bb21ee8feddbdeef080493b19a65))
* **engine:** parallelize type environment construction with rayon ([b92a7c4](https://github.com/Kehl-io/nestweaver/commit/b92a7c48dbb693439ea3c1f5f9e3f8a364375f1e))
* **engine:** retain parsed source to avoid redundant disk re-reads ([0028f87](https://github.com/Kehl-io/nestweaver/commit/0028f87fc8d8f5281b567e707aa105231aee56aa))
* **engine:** use bulk delete for force re-index cleanup ([42be7a5](https://github.com/Kehl-io/nestweaver/commit/42be7a53d803b43a3c529a0a6d686f3ed553d060))
* **mcp:** hold response cache in-process with periodic flush ([bebc4d3](https://github.com/Kehl-io/nestweaver/commit/bebc4d3aad6af6c2ff2e663bd63ffa2bda104878))
* **parser:** cache compiled tree-sitter Query objects globally ([31dc3f5](https://github.com/Kehl-io/nestweaver/commit/31dc3f5082eb1d562bcb3f0fa1cb46245a9ff611))
* **resolver:** parallelize per-file reference resolution with rayon ([d67f3b4](https://github.com/Kehl-io/nestweaver/commit/d67f3b4e3f8437a9c630cc6b39e5582078919b74))
* **resolver:** use binary search for find_enclosing_symbol (O(n) → O(log n)) ([2423699](https://github.com/Kehl-io/nestweaver/commit/2423699a8c27f7914a7ecf8896600584db0d2278))
* **store:** add _on variants for markdown batch inserts ([d77e747](https://github.com/Kehl-io/nestweaver/commit/d77e747ba91dcf215a1309ad0d91c0f5b8eceaa1))
* **store:** batch symbol lookups to eliminate N+1 after PPR ([4220cad](https://github.com/Kehl-io/nestweaver/commit/4220cad448e2b75f1b22cb040d77ea1ff6a47198))
* **store:** batch Tantivy commits and use bulk section/heading loaders ([43468ed](https://github.com/Kehl-io/nestweaver/commit/43468ed0453127c2f9d6b68c712af451735edefc))
* **store:** cache PPR adjacency graph keyed on (generation, scope, intent) ([ffd3fba](https://github.com/Kehl-io/nestweaver/commit/ffd3fba5f631c61b66cea8d0203ecc38a929b694))
* **store:** cache symbol name index keyed on graph generation ([c577725](https://github.com/Kehl-io/nestweaver/commit/c5777259d56cc401150c101ff7598310c6dea6ee))
* **store:** replace per-row cascade deletes with bulk DETACH DELETE ([e6df168](https://github.com/Kehl-io/nestweaver/commit/e6df1687ff69f8ff18503560fb8a4367f74acc87))
* **store:** switch response cache to binary format (MessagePack + ZSTD) ([7fed80d](https://github.com/Kehl-io/nestweaver/commit/7fed80d507d175516d1bc46be578f666b6b96c17))
* **store:** wrap vault indexing in single transaction via bulk_vault_write ([9123576](https://github.com/Kehl-io/nestweaver/commit/9123576431dcefaf16834cb6326d5210915b5178))

## [0.19.0](https://github.com/Kehl-io/nestweaver/compare/v0.18.0...v0.19.0) (2026-06-05)


### Features

* **cli:** instance_id from config, instance merge, and duplicate vault warning ([160d40e](https://github.com/Kehl-io/nestweaver/commit/160d40e65a9149ffaddf28dd4ecc960dda59f142))


### Bug Fixes

* **ci:** eliminate zstd link errors and skip daemon tests on Linux ([c017d0f](https://github.com/Kehl-io/nestweaver/commit/c017d0f8b0dcfe13b47ce63de679ab7157958c67))
* **cli:** add stale-check subcommand, body_complete indicator, and snapshot output default ([ffce373](https://github.com/Kehl-io/nestweaver/commit/ffce3734b99f6db230339f4331acc9d4fdcf247c))
* **store:** deduplicate vaults during instance merge ([16bf188](https://github.com/Kehl-io/nestweaver/commit/16bf18803f463c9cb05b7f24bc283979bf35d01e))
* **store:** make insert_vault idempotent and remove obsolete binary ([494caae](https://github.com/Kehl-io/nestweaver/commit/494caaef0bbf2fcb6534e36c52d1f8d345b082b2))
* **store:** use DETACH DELETE pattern in merge_instance_ids ([dbd8bff](https://github.com/Kehl-io/nestweaver/commit/dbd8bff178acd5890e9b343d933b1af85a8413b4))
* **store:** use upsert_vault with DETACH DELETE pattern ([b9bd9c0](https://github.com/Kehl-io/nestweaver/commit/b9bd9c01afbfd225d096864ace99291f4b0f8f4e))
* vault idempotency, stale-check CLI, instance merge, and CI linker fix ([959d6ac](https://github.com/Kehl-io/nestweaver/commit/959d6aca05531b3aa0fa94e53b32e4dff28d72d6))

## [0.18.0](https://github.com/Kehl-io/nestweaver/compare/v0.17.0...v0.18.0) (2026-06-04)


### Features

* **cli:** route `brain search` through daemon Search RPC when available ([8bb519d](https://github.com/Kehl-io/nestweaver/commit/8bb519df69364ce4eb5297e700c1a3088f0ad692))
* **daemon:** load InstanceConfig for ranking-prior parity in tool dispatch ([c44ec9c](https://github.com/Kehl-io/nestweaver/commit/c44ec9c6c90a1a957bdb5532cd4bd2486d093766))
* **investigate:** surface body truncation via `body_complete` + newline-aware cuts ([c3cf283](https://github.com/Kehl-io/nestweaver/commit/c3cf283c5d59b4d09642fe8dc5011b3e6eefe2b5))


### Bug Fixes

* **ci:** add --workspace to clippy job and fix pre-existing test errors ([a50b217](https://github.com/Kehl-io/nestweaver/commit/a50b217a0300298c7b885219cf98cedddca028d6))
* **cli:** atomically detect running daemon via pidfile flock in daemon start ([b741b94](https://github.com/Kehl-io/nestweaver/commit/b741b94540e36e5b3adf543a5c12329b9f88e8f2))
* **cli:** silence stop_watch errors when daemon is already gone ([1dc8e3e](https://github.com/Kehl-io/nestweaver/commit/1dc8e3e8b7f4b443e310727de589f9e23df28175))
* **cli:** thread --config through autostart so auto-spawned daemons get ranking priors ([1d11e6d](https://github.com/Kehl-io/nestweaver/commit/1d11e6dcb262b4bf3bd16409ae00b0fa453248c3))
* daemon correctness, search parity, investigate fidelity, and agentic integration improvements ([b98069e](https://github.com/Kehl-io/nestweaver/commit/b98069ee5697890a27f1be4a7b3a76a47ed91e31))
* **engine:** route daemon writer-mode Tantivy index into BrainWatcher ([9681fb5](https://github.com/Kehl-io/nestweaver/commit/9681fb5b985822e993ff16204a524b35f495486a))
* **search:** treat `limit` as per-kind so symbols stop being squeezed out ([f37a0bd](https://github.com/Kehl-io/nestweaver/commit/f37a0bd3a51dbd9eb6308aff4945c514eebddb11))

## [0.17.0](https://github.com/Kehl-io/nestweaver/compare/v0.16.0...v0.17.0) (2026-06-04)


### Features

* edge evidence arrays + full language parity (27 tree-sitter, 19 type queries) ([c7a955d](https://github.com/Kehl-io/nestweaver/commit/c7a955d7e636619a57ea5c295adc6c43b9455867))
* **export:** read and surface edge evidence in cypher export ([4564879](https://github.com/Kehl-io/nestweaver/commit/456487924926827d54cdbc6da499bfdb3b6c7dd8))
* **parser:** add C/Elixir type queries, fix Swift receivers, add Lua self binding ([be91ebc](https://github.com/Kehl-io/nestweaver/commit/be91ebc0ff478dc57aee3648c8bb1ddc7ccfd146))
* **parser:** add constructor patterns to Kotlin, Dart, Swift, Scala, C#, SystemVerilog type queries ([8f6703e](https://github.com/Kehl-io/nestweaver/commit/8f6703eeae64d1683e3ab65cd377a6824babab2f))
* **parser:** add Go interface methods, array types + Python class attributes, instance properties ([59afd0b](https://github.com/Kehl-io/nestweaver/commit/59afd0b0de0d8bc0119da275c476d5d14b715104))
* **parser:** add interface method declarations and instance property captures across languages ([3ad76ae](https://github.com/Kehl-io/nestweaver/commit/3ad76aec180cc399e24c7a726a44acabebf4180b))
* **parser:** add type extraction queries for C++, C#, Kotlin, PHP, Dart, Swift, Scala, Ruby ([723b6ac](https://github.com/Kehl-io/nestweaver/commit/723b6acb7f9879b137bc33f0c9b43f64053a0b77))
* **parser:** add type queries and self bindings for remaining OOP languages ([5304c7b](https://github.com/Kehl-io/nestweaver/commit/5304c7b97ab9c987bbfe2b00e58a8f90cbeb6a70))
* **parser:** add visibility inference for Ruby, PowerShell, Fortran, Pascal, SystemVerilog, Julia ([fd2a360](https://github.com/Kehl-io/nestweaver/commit/fd2a36016093b26c052a5bb8b003e904c8c620b7))
* **parser:** expand receiver extraction to all languages for type_aware resolution ([e633458](https://github.com/Kehl-io/nestweaver/commit/e633458a12dd423b3f403395284cd026cda54842))
* **parser:** extend parent_name to fields/properties for MEMBER_OF expansion ([3727cc9](https://github.com/Kehl-io/nestweaver/commit/3727cc977d56af69c4c4c3643fd808662a56cd7e))
* **parser:** upgrade Fortran, Pascal, SystemVerilog from regex to tree-sitter ([47fc325](https://github.com/Kehl-io/nestweaver/commit/47fc32535c3a69f77b7eb77f5037a3299bb143c6))
* **parser:** upgrade Groovy, Zig, Objective-C from regex to tree-sitter ([66e0353](https://github.com/Kehl-io/nestweaver/commit/66e0353c10613543dcc44cc602457f5628390e99))
* **parser:** upgrade PowerShell, Julia, SQL, HCL from regex to tree-sitter ([c79c268](https://github.com/Kehl-io/nestweaver/commit/c79c268d7db4288edb4ab43a4009dffd875d751a))
* **resolver:** add import resolvers for Scala, Groovy, Fortran, Pascal, SystemVerilog, Zig, ObjC, Lua, PowerShell, Julia ([1e44272](https://github.com/Kehl-io/nestweaver/commit/1e44272d962f3b9e0be306caeba9af6d7d9e4d4a))
* **resolver:** improve assignment extraction with dotted paths and function calls ([9ac9e4f](https://github.com/Kehl-io/nestweaver/commit/9ac9e4f3d0cb1a02a6d686d9886fbe8f08843535))
* **resolver:** populate evidence array at each resolution step ([0ecd153](https://github.com/Kehl-io/nestweaver/commit/0ecd153584b4d2cc0837b1294a1edabc039d5760))
* **schema:** add EdgeEvidence struct and evidence field to ResolvedEdge ([b4513aa](https://github.com/Kehl-io/nestweaver/commit/b4513aa1a7ff0b34f52bfff5f106464cb7415d18))
* **store:** persist edge evidence as JSON property ([a268bcd](https://github.com/Kehl-io/nestweaver/commit/a268bcdfbb0eff90bd421397cd21591d908d37ae))
* **store:** surface FILE_HAS_SYMBOL as DEFINES edges in typed edge export ([049d745](https://github.com/Kehl-io/nestweaver/commit/049d745793d81e681d6a3ead2a5b93f51a5247b4))


### Bug Fixes

* add evidence to store/ranking tests, remove unused tree-sitter-sql dep ([b03a828](https://github.com/Kehl-io/nestweaver/commit/b03a8281b35f81af87be9834d5af6942620fabe6))
* address all minor review findings — deterministic resolvers, Ruby visibility docs, ObjC fallback docs, fix dotted assignment test ([9cac397](https://github.com/Kehl-io/nestweaver/commit/9cac397873e8d86491ce4d68901fe0c61a5639eb))
* address code review findings — remove misleading Julia visibility, fix self-referential assignments, remove redundant SV check ([3b593f2](https://github.com/Kehl-io/nestweaver/commit/3b593f2a73540277822bf411453ab5acca1bc8ef))
* implement real Elixir type extraction and re-enable HCL tree-sitter ([1a42390](https://github.com/Kehl-io/nestweaver/commit/1a42390ec03d4523e584666a42064bd544998226))
* resolve clippy warnings and remove dead regex parser files ([194d79a](https://github.com/Kehl-io/nestweaver/commit/194d79ad05f0167149e2638f20ad4f1c17827ca0))
* **resolver:** deterministic import resolution — prefer shortest path on ambiguous matches ([e845cb8](https://github.com/Kehl-io/nestweaver/commit/e845cb85b61c4b2344c2f92c156c20c30b286bc5))
* revert HCL to regex parser, fix Swift types query, relax Julia/PowerShell tests ([60d5bb7](https://github.com/Kehl-io/nestweaver/commit/60d5bb7268dbb99797ce47972429805e82b24617))
* rustfmt formatting ([d32025d](https://github.com/Kehl-io/nestweaver/commit/d32025d705c0030e7728984c97f4c7bf138f6715))
* rustfmt imports.rs ([ea2b21a](https://github.com/Kehl-io/nestweaver/commit/ea2b21a6fce7ca555a10edc1ffe42f216b60d1cd))
* use TypedEdge type alias to satisfy clippy complexity lint ([a881c47](https://github.com/Kehl-io/nestweaver/commit/a881c4728be7c9df20508194025c800e095a7ae1))

## [0.16.0](https://github.com/Kehl-io/nestweaver/compare/v0.15.0...v0.16.0) (2026-06-02)


### Features

* add watch config, simplify daemon lock error, apply lint fixes ([f9e1bf9](https://github.com/Kehl-io/nestweaver/commit/f9e1bf9129fac29101a5661c4c90e3381f5d21b3))
* **cli:** route brain watch through daemon when use_daemon=true ([78e2a1d](https://github.com/Kehl-io/nestweaver/commit/78e2a1d5955d3eaaae2e09dc77b018bba9877b82))
* **daemon:** implement WatchVault and StopWatch RPCs ([199cab1](https://github.com/Kehl-io/nestweaver/commit/199cab18e034f2f2a169044d1494463a0dd5abb7))
* implement stubbed features, daemon-subsumes-watcher, production hardening ([5542a6e](https://github.com/Kehl-io/nestweaver/commit/5542a6e0324ec4f67a2218a8908a8fbc19f879a9))
* **index:** emit MEMBER_OF edges from parent_name during indexing ([2f516a9](https://github.com/Kehl-io/nestweaver/commit/2f516a9ec03e3548125a0e0ff7359322f70b8f72))
* **parser:** expand Rust type queries with struct constructors and destructuring ([533feec](https://github.com/Kehl-io/nestweaver/commit/533feec02bfaf6c1d260dd4d0f7eecbbabb59539))
* **parser:** expand type queries for TS, Python, Go, Java ([e29b916](https://github.com/Kehl-io/nestweaver/commit/e29b9163e8c8c01909a97a79eea91d15e8e06cc7))
* **proto:** add WatchVault and StopWatch RPCs ([90b04c2](https://github.com/Kehl-io/nestweaver/commit/90b04c2709bc22b321352e11162e571b0c4ef4de))
* **resolver:** decompose chained dot receivers for type-aware resolution ([66be11d](https://github.com/Kehl-io/nestweaver/commit/66be11d9d4ec7b6be4293c49479228b2fc767fcd))


### Bug Fixes

* address final review minor items ([a3c9145](https://github.com/Kehl-io/nestweaver/commit/a3c91450365c4cd93883d8ce0a43fc06bd1e82c1))
* address review findings for production readiness ([48c3171](https://github.com/Kehl-io/nestweaver/commit/48c31716c6c886a99730cb5b4260af3f7d882128))
* **snapshot:** query actual embedding dimension from store ([f72b923](https://github.com/Kehl-io/nestweaver/commit/f72b923b1993e9f48bea04bb52be2549b24d59ba))

## [0.15.0](https://github.com/Kehl-io/nestweaver/compare/v0.14.0...v0.15.0) (2026-06-02)


### Features

* **cli:** implement snapshot build command ([04d2085](https://github.com/Kehl-io/nestweaver/commit/04d20856b076c5f7e59850e0e9f620fc29ec0cd7))
* **cli:** implement snapshot push command ([52930c0](https://github.com/Kehl-io/nestweaver/commit/52930c046eabdabe1fd9a54701cf38deff530d26))
* **engine:** implement memory consolidate --apply ([24fda7a](https://github.com/Kehl-io/nestweaver/commit/24fda7a72dda6d4b4211618fdc9329e9f76182e7))
* **engine:** rewrite wikilinks after consolidate --apply moves files ([228f95f](https://github.com/Kehl-io/nestweaver/commit/228f95f7f24e158bbe524d8fc81cded29832ebf0))
* implement stubbed snapshot and consolidate features ([fff4504](https://github.com/Kehl-io/nestweaver/commit/fff4504ebb3058490c95382cf3252650fd7b929c))
* **snapshot:** switch to per-file checksums covering sidecars ([c385735](https://github.com/Kehl-io/nestweaver/commit/c385735a86aacee1b1cccfec4ef517117aa8706f))


### Bug Fixes

* address code review findings ([2b25c5a](https://github.com/Kehl-io/nestweaver/commit/2b25c5a827b0be214fe9ea8b27113eb4dc4dbcfd))
* apply rustfmt and clippy fixes ([308e2a7](https://github.com/Kehl-io/nestweaver/commit/308e2a70d6aafb7de998f9e34b61375380fb4706))
* **cli:** verify snapshot integrity after instance pull ([518317d](https://github.com/Kehl-io/nestweaver/commit/518317dccab6a9a0c78997727f991fb40b66451e))
* **snapshot:** decouple min_compatible_engine from build version ([8fe7b8a](https://github.com/Kehl-io/nestweaver/commit/8fe7b8a5f620f33b392b8dd0c2b621d86186b267))
* **storage:** LocalBackend push/pull now copies subdirectories ([af9c0a1](https://github.com/Kehl-io/nestweaver/commit/af9c0a111ceaba4f6c5702b7db89f5556ddf7a08))

## [0.14.0](https://github.com/Kehl-io/nestweaver/compare/v0.13.0...v0.14.0) (2026-06-02)


### Features

* AST-based type extraction via tree-sitter queries (zero re-parse cost) ([a47cf37](https://github.com/Kehl-io/nestweaver/commit/a47cf371d03f43a0d2ef265917a091dad1c1de81))
* **parser:** add tree-sitter type query files for 5 languages ([8e2c0c1](https://github.com/Kehl-io/nestweaver/commit/8e2c0c18735007a628876b192627093bc8dcacd6))
* **parser:** walk tree-sitter AST for type bindings (zero re-parse cost) ([076bc30](https://github.com/Kehl-io/nestweaver/commit/076bc30bbbd9b4c789c6dbf41b4ad6cdd7b52b56))
* **resolver:** feed AST type bindings into TypeEnvironment as primary source ([3c431e4](https://github.com/Kehl-io/nestweaver/commit/3c431e4dfc670dba8129bab3931b349543a129bd))


### Bug Fixes

* collapse nested if-let per clippy::collapsible_if ([57bc87a](https://github.com/Kehl-io/nestweaver/commit/57bc87a5d9289988b59189c4d8763a08ec1e322a))
* critical bugs batch 2 — daemon fresh-DB, skill generation, UTF-8 panic, write-routing ([c7ecc09](https://github.com/Kehl-io/nestweaver/commit/c7ecc0950d339382fd04cf3d50fa7504971865d9))
* critical bugs batch 2 — daemon fresh-DB, skill tool count, daemon write-routing ([b17f4f3](https://github.com/Kehl-io/nestweaver/commit/b17f4f3ab8e255ab3a519817cc674ee77a0a53b2))
* rustfmt formatting ([e4a9199](https://github.com/Kehl-io/nestweaver/commit/e4a9199fc071663ccd533a0529b8ac1974fc4469))
* UTF-8 char-boundary panic in type extractors + daemon fresh-DB canonicalize ([f1f798b](https://github.com/Kehl-io/nestweaver/commit/f1f798b0c63f0e0303ee5a1d7a07c76236b98a94))

## [0.13.0](https://github.com/Kehl-io/nestweaver/compare/v0.12.1...v0.13.0) (2026-06-01)


### Features

* **blast_radius:** confidence-weighted edge traversal for impact analysis ([dca11a8](https://github.com/Kehl-io/nestweaver/commit/dca11a8c781529fb2020f4a4be84bde29e9939da))
* **daemon:** set process title to nestweaver-daemon-{id} for pgrep ([4f1dc4c](https://github.com/Kehl-io/nestweaver/commit/4f1dc4c3a8d80e80311b5546cb0cf940616ae9e1))
* **engine:** co-change mining from git history with Jaccard scoring ([2c3286b](https://github.com/Kehl-io/nestweaver/commit/2c3286bae2dde22fb5840fa9ba297ebfbae5e6aa))
* **index:** build TypeEnvironments per file during indexing (not yet used for resolution) ([bb59ca6](https://github.com/Kehl-io/nestweaver/commit/bb59ca6159aa53e244d17d1aac4439a629930dc3))
* **parser:** extract parent class/struct name for method symbols ([72e94e3](https://github.com/Kehl-io/nestweaver/commit/72e94e304580f7e394c7e3299f60e80f084f4a33))
* **parser:** extract receiver from method call expressions ([53fb5d2](https://github.com/Kehl-io/nestweaver/commit/53fb5d2b9980b373f935439c1ef70e6d08c0fa6b))
* **resolver:** add per-language type extractors for annotations and constructors ([bb3fba4](https://github.com/Kehl-io/nestweaver/commit/bb3fba4a1ecd0a6a7c6473eae33c334a4f36f078))
* **resolver:** cross-file return type propagation for type-aware resolution ([d2fd4bf](https://github.com/Kehl-io/nestweaver/commit/d2fd4bf86e78a23b071dcf054f5c5e52c5492941))
* **resolver:** MRO walk for inherited methods in type-aware resolution ([9b03f97](https://github.com/Kehl-io/nestweaver/commit/9b03f970f3eff541cf3827da1267504015ca37b2))
* **resolver:** type-aware member call resolution using TypeEnvironment ([1db986d](https://github.com/Kehl-io/nestweaver/commit/1db986d350cbb23d4c789d4719f52c20e88b9d95))
* **resolver:** TypeEnvironment with 4-tier inference and fixpoint propagation ([d0f8136](https://github.com/Kehl-io/nestweaver/commit/d0f8136d04368dacff82b2e8609a09d47e3d62cb))
* type-aware call resolution, confidence-weighted impact analysis, co-change mining ([8c82ba6](https://github.com/Kehl-io/nestweaver/commit/8c82ba62eab6d0f955f6a64de05538c4ec56a290))


### Bug Fixes

* **cli:** allow --db after daemon subcommand with global arg ([0549139](https://github.com/Kehl-io/nestweaver/commit/05491397ded1fb8ea767ad2c284fdecb7b864962))
* **daemon:** shorten socket path to fit macOS 104-byte sun_path limit ([528302a](https://github.com/Kehl-io/nestweaver/commit/528302a8894753faa691965537ab5e03d972524b))
* derive tool documentation from registry instead of hardcoded tables ([99f43b2](https://github.com/Kehl-io/nestweaver/commit/99f43b2d771448f463a5977afc26da09a52e2082))
* resolve all clippy warnings in resolver crate ([c9f57d5](https://github.com/Kehl-io/nestweaver/commit/c9f57d581d2bf4a542308942efbe2d64e9b62135))
* **setup:** protect existing skill/rule files from overwrite ([9675d75](https://github.com/Kehl-io/nestweaver/commit/9675d75cf7df05c599183233a25d628f0d63fd9a))
* use multiplicative confidence decay instead of linear (research-backed) ([9d38ecc](https://github.com/Kehl-io/nestweaver/commit/9d38ecc7be6914e41951a4038a66bc5dee8213a3))
* warn on deprecated --allow-mcp-add-sources and strip from existing configs ([4a405c1](https://github.com/Kehl-io/nestweaver/commit/4a405c10b5dd0200a73cda532d4d5f084ee0f1a1))

## [0.12.1](https://github.com/Kehl-io/nestweaver/compare/v0.12.0...v0.12.1) (2026-06-01)


### Bug Fixes

* **ci:** add protobuf-compiler to release build workflow ([534a2f3](https://github.com/Kehl-io/nestweaver/commit/534a2f333817a10dd43b26f67c7063dc66166a32))

## [0.12.0](https://github.com/Kehl-io/nestweaver/compare/v0.11.0...v0.12.0) (2026-06-01)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* agent guidance — hard rules in guides + subagent hook (F14, F15) ([0f1e9a2](https://github.com/Kehl-io/nestweaver/commit/0f1e9a2de2106bba5a3d81852f7b81cca3879f8a))
* agent interaction memory — PPR bias from usage patterns ([3e568f6](https://github.com/Kehl-io/nestweaver/commit/3e568f638c09f6f2133731820bcd2048e1179e9b))
* **algorithms:** add impact analysis BFS and substring search ([54164e0](https://github.com/Kehl-io/nestweaver/commit/54164e07e9fb9ce79345dedbd6ac6dd1ae2c9a40))
* **algorithms:** create nestweaver-algorithms crate with InMemoryGraph and PPR ([d3d2941](https://github.com/Kehl-io/nestweaver/commit/d3d294134ffa6ea0fa770c2d08562070267a6836))
* API contract graph — Contract nodes, IMPLEMENTS edges, drift (F2-core) ([70fd961](https://github.com/Kehl-io/nestweaver/commit/70fd9616929d5e124d0f95d82d5e681f4898518b))
* brain.* document-graph tools (F9) ([f095275](https://github.com/Kehl-io/nestweaver/commit/f0952759762f559836708d1e82c3b24db681be42))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add --allow-writes flag to setup command ([62d06e2](https://github.com/Kehl-io/nestweaver/commit/62d06e2587470d5ce7d7b2adfee1cfecdb60f9af))
* **cli:** add --daemon flag to route index through gRPC daemon ([96e707d](https://github.com/Kehl-io/nestweaver/commit/96e707d9fc44b59940096366317ddb3c0245b4a7))
* **cli:** add --name flag for repo display name override ([52e1c05](https://github.com/Kehl-io/nestweaver/commit/52e1c05a6a20a1d529e27301e8ff2eb44d08be48))
* **cli:** add --stats flag and output control flags ([52449fb](https://github.com/Kehl-io/nestweaver/commit/52449fb46b96360e7a7cab9b97dff43c5a9f2f25))
* **cli:** add --tools flag for MCP tool allowlisting ([9d3c419](https://github.com/Kehl-io/nestweaver/commit/9d3c4191e7a7ae42065230a2e64be43e79907be3))
* **cli:** add daemon start/stop/status subcommands ([b567702](https://github.com/Kehl-io/nestweaver/commit/b567702a31be4156d8554e269692239e7df6cf77))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **cli:** add shell completions, miette diagnostics, and zero-config index ([e483aaf](https://github.com/Kehl-io/nestweaver/commit/e483aaf735dad9875ce77b7e5da167671deed0d3))
* **cli:** add standard output control flags ([60b218a](https://github.com/Kehl-io/nestweaver/commit/60b218ad46ce414934836aad8b9c52bd78939dfd))
* **client:** add nestweaver-client crate with auto-start and version check ([43560a4](https://github.com/Kehl-io/nestweaver/commit/43560a4c554a333c3821e332ca41eb3df7604eaa))
* **cli:** expose materialize-projects and detect-implicit-projects as subcommands ([1c03b29](https://github.com/Kehl-io/nestweaver/commit/1c03b29fe0aea75c62637cd3e8a33e56b5a6378f))
* **cli:** restore materialize-projects and detect-implicit-projects subcommands ([a86c787](https://github.com/Kehl-io/nestweaver/commit/a86c7877e5f211e4884bf251e879e578fde089e0))
* **cli:** show declared projects from --config alongside materialized ones in list-projects ([f336f93](https://github.com/Kehl-io/nestweaver/commit/f336f9314027fe5e35a89a67c12dd814c8e81e57))
* daemon-based concurrent database access with performance optimizations ([a54c2a9](https://github.com/Kehl-io/nestweaver/commit/a54c2a9f082ab8bb5598f754c9fc51ac4c71827e))
* **daemon:** add nestweaver-daemon crate with gRPC server scaffold ([a608a36](https://github.com/Kehl-io/nestweaver/commit/a608a36d6429d5f8867a432675ac4163a86f037d))
* **daemon:** daily log rotation via tracing-appender with non-blocking writer ([c254458](https://github.com/Kehl-io/nestweaver/commit/c254458a2b52b49f087a96296686c0f48a85f951))
* **daemon:** implement IndexRepo and IndexVault streaming RPCs ([44940c2](https://github.com/Kehl-io/nestweaver/commit/44940c278dbb81c5f3ec1e88d22412b11069f6b8))
* default MCP and CLI to daemon mode, add --no-daemon escape hatch ([b3ff013](https://github.com/Kehl-io/nestweaver/commit/b3ff0138461f9105582400dca33d7b7de232fe89))
* **engine:** add .brainignore support for vault indexing exclusion patterns ([9effc18](https://github.com/Kehl-io/nestweaver/commit/9effc18f741d557520232232e72db31de4f32cfd))
* **engine:** add dead code detection via entry point reachability ([5f4221c](https://github.com/Kehl-io/nestweaver/commit/5f4221c81fa807f4a4ec6f6e8a321cf1a3e0b698))
* **engine:** add graph export to Cypher, GraphML, and Mermaid ([0a9e5f8](https://github.com/Kehl-io/nestweaver/commit/0a9e5f8e24b5b21795cb9a824f9f492a71267862))
* **engine:** add hierarchical code summaries for token-efficient retrieval ([9a26820](https://github.com/Kehl-io/nestweaver/commit/9a268209e2d31d424181646e33cf748bfac7e9b7))
* **engine:** add HTML-to-markdown conversion for wiki content ingestion ([28b071f](https://github.com/Kehl-io/nestweaver/commit/28b071f3f366925b44a18cc21790821228d9be43))
* **engine:** add hub and bridge node detection ([30de53d](https://github.com/Kehl-io/nestweaver/commit/30de53d2c9be7da84c8d15e6a1d8e14051ecaa87))
* **engine:** add InteractionTracker with event recording, consolidation, and decay ([175b54c](https://github.com/Kehl-io/nestweaver/commit/175b54c530826d9fb03c3f51de4012e3d7e153ef))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add PR blast radius analysis with risk scoring ([70e2b91](https://github.com/Kehl-io/nestweaver/commit/70e2b9119e14444f1fda2bfac0b4857dd6eab65e))
* **engine:** add progress bars to indexing pipeline ([e328690](https://github.com/Kehl-io/nestweaver/commit/e328690df1de77f1286d4db710ab0cc189fae4c6))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** add read_symbols — symbol-window source reads (F5) ([2759894](https://github.com/Kehl-io/nestweaver/commit/2759894df248da9c99c46d70dc35d8167e635a87))
* **engine:** add setup support for 10 additional AI tools ([1daf7d2](https://github.com/Kehl-io/nestweaver/commit/1daf7d213395894953efab33cea22eea9748d2e7))
* **engine:** add watch mode for live code re-indexing ([13e2fba](https://github.com/Kehl-io/nestweaver/commit/13e2fba37bb6c93743c87f02992bd2dd537a1d07))
* **engine:** affected_tests — static RTS for PR test selection (F13) ([0965cbd](https://github.com/Kehl-io/nestweaver/commit/0965cbde7db2d7962642a7c03fed6cc853373622))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** expand brain_search queries with taxonomy aliases for better recall ([2eaa63b](https://github.com/Kehl-io/nestweaver/commit/2eaa63b9abc3a99399485b9362b2d9823558f434))
* **engine:** finish agent feedback loop — TerminalSuccess + interactions show (F1) ([cfd0120](https://github.com/Kehl-io/nestweaver/commit/cfd0120d8ffbda88faea783ee1954de69c13d1a8))
* **engine:** generate SKILL.md conforming to Agent Skills standard ([a4c2e38](https://github.com/Kehl-io/nestweaver/commit/a4c2e38798698cb0fce8830d0a4cc62ff524bcb4))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **engine:** inline high-confidence result bodies (F8) ([6543f72](https://github.com/Kehl-io/nestweaver/commit/6543f720737eb8522f984f6dd7dd1fd06807f706))
* **engine:** investigate bundle primitive (F10) ([03f765e](https://github.com/Kehl-io/nestweaver/commit/03f765ec5233bf3ac8f93a1049330d184ddf6c92))
* **engine:** lightweight result reranker (F17) ([55bc2f8](https://github.com/Kehl-io/nestweaver/commit/55bc2f8b1bca17301a214bcbd0ccc6b692470a43))
* **engine:** per-path dampen/boost ranking priors (F6) ([c183679](https://github.com/Kehl-io/nestweaver/commit/c183679cba79011dba4517a7a784a9363b2fc278))
* **engine:** retrieval-quality eval harness (P0.3) ([be5955f](https://github.com/Kehl-io/nestweaver/commit/be5955f86f99fea79bbceaf1e1a3e41b42ccd966))
* **engine:** unify brain_search to return both vault notes and code symbols ([db07420](https://github.com/Kehl-io/nestweaver/commit/db07420e310ad86484531dc41e3bcfd9d10e954b))
* git-activity-dampened CodeRank (F12) ([61faf2d](https://github.com/Kehl-io/nestweaver/commit/61faf2d26e912d4c208082955ac4c35e3c34888a))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **mcp:** add daemon proxy mode with --daemon flag ([a74e71c](https://github.com/Kehl-io/nestweaver/commit/a74e71c26224551ca5743f62b16609f763f4aca3))
* **mcp:** add intent to project_context, tool allowlist, and section title indexing ([36b238e](https://github.com/Kehl-io/nestweaver/commit/36b238e22d07b89b7f45f058d640792f315e90a4))
* **mcp:** add interaction telemetry hooks to MCP tool dispatch ([207b4cc](https://github.com/Kehl-io/nestweaver/commit/207b4cc66c8de2c26b3dffc4f731f2aeb2f62cce))
* **mcp:** improve tool descriptions and add response_format parameter ([d178db1](https://github.com/Kehl-io/nestweaver/commit/d178db1aed20d6d29684d2e7eb0a4f48977ce691))
* **mcp:** wire intent through brain_context and cache get_summary in sidecar ([626918b](https://github.com/Kehl-io/nestweaver/commit/626918b10e8347d15c179b8f3c9f3ae8d6254194))
* memory-bank semantics — typed edges, lint, consolidate, related (F11) ([8d35b37](https://github.com/Kehl-io/nestweaver/commit/8d35b37d552223d3a7a17f8b125c6134a7c4523f))
* next-gen R3F web UI, algorithms/WASM crates, and v0.9.1 retrieval quality + eval harness ([028d62f](https://github.com/Kehl-io/nestweaver/commit/028d62f0eb5de29ad112c6a309995087143ce604))
* **parser:** add JSX component edges and confidence-aware dead code BFS ([965e4c7](https://github.com/Kehl-io/nestweaver/commit/965e4c74bcef917e24a252b753b6e611263933fa))
* **parser:** add Julia, SQL, HCL, Fortran, and Pascal language support ([9638978](https://github.com/Kehl-io/nestweaver/commit/9638978fef35ca8797cbfb1fe3c5a63351ab80d2))
* **parser:** add Lua, Bash, Scala, and Elixir language support ([d3556c1](https://github.com/Kehl-io/nestweaver/commit/d3556c1cfec308d64c4f6d3772d8b209ebb50ae7))
* **parser:** add Vue, Svelte, Astro, and SystemVerilog language support ([97509e3](https://github.com/Kehl-io/nestweaver/commit/97509e37d72f48b7666079689d2a84ecb6851355))
* **parser:** add Zig, Objective-C, Groovy, and PowerShell language support ([d4472c5](https://github.com/Kehl-io/nestweaver/commit/d4472c584b81b84528eca76d54396b328cafb8ad))
* **parser:** enrich symbol extraction with constants, properties, types, and expanded queries ([201c27d](https://github.com/Kehl-io/nestweaver/commit/201c27d67b7a292f29e3fdbfc405f015518c2298))
* persisted graph_generation (P0.2) + ZSTD response cache (F16) ([283e352](https://github.com/Kehl-io/nestweaver/commit/283e352fa37b4f7aafc7d951ad989571f7174e0a))
* **proto:** add nestweaver-proto crate with daemon gRPC service definition ([b6d8c11](https://github.com/Kehl-io/nestweaver/commit/b6d8c1192be0c0730483efbb434edefe24091ea0))
* **proto:** typed protobuf messages for 6 hot-path RPCs ([3165186](https://github.com/Kehl-io/nestweaver/commit/316518643f80ad55519ae91f3b070c49b5faa01d))
* **resolver:** add file-level IMPORTS edges for every resolved import ([7ce74dc](https://github.com/Kehl-io/nestweaver/commit/7ce74dcfa7ee49296fc0554aedafd64963086fda))
* **resolver:** add monorepo workspace package and tsconfig path alias resolution ([2eff955](https://github.com/Kehl-io/nestweaver/commit/2eff955fe63bc7e1970f9f8bc1665de410a93a38))
* **resolver:** create IMPORTS edges from resolved import references ([5e65cf5](https://github.com/Kehl-io/nestweaver/commit/5e65cf5314617a465b8a2b451201a6f82c8ae4ec))
* **schema:** add Symbol.end_line spanning a symbol's full source range (P0.1) ([45abd76](https://github.com/Kehl-io/nestweaver/commit/45abd764fe51a95b32ec41591fe509e87119e840))
* **schema:** add USES and ACCESSES edge types with PPR weighting and workspace resolution ([8ab7c21](https://github.com/Kehl-io/nestweaver/commit/8ab7c21a57a00e2ad3870569b11d0f5efc212abb))
* **store:** add graph_generation counter and watcher shared-store support ([d9bc01d](https://github.com/Kehl-io/nestweaver/commit/d9bc01d8416aa6fdd348a302cd2d31be865aac55))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **store:** add upsert helpers for idempotent project materialization ([1c24c4f](https://github.com/Kehl-io/nestweaver/commit/1c24c4ffaaab90a1f4c733003c26985d72b574dd))
* **store:** BM25 pseudo-relevance feedback (PRF) (F7) ([a950cac](https://github.com/Kehl-io/nestweaver/commit/a950cac348c8678d34de899eb99b7a79109d6391))
* **store:** dynamic PPR with query-type-aware alpha and edge weighting ([55123a3](https://github.com/Kehl-io/nestweaver/commit/55123a3d062c3eb091a81fca716e300d14587877))
* **store:** trigram-accelerated regex_search + count_patterns (F3, F4) ([efaa376](https://github.com/Kehl-io/nestweaver/commit/efaa376ec7aad2bcda90af809ff6b86a7fe8761e))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))
* **wasm:** add nestweaver-wasm crate, msgpack export, and snapshot endpoint ([c94cd38](https://github.com/Kehl-io/nestweaver/commit/c94cd3842027181515026493cc6e14e6e1ae9447))
* **wasm:** build WASM binary and wire worker/bridge to real wasm-bindgen API ([afb466f](https://github.com/Kehl-io/nestweaver/commit/afb466fd2ed9b1ce860aa2b4d86b43c2d250bcf1))
* **web:** add --watch flag for live re-indexing and /api/v1/version endpoint ([e3d14be](https://github.com/Kehl-io/nestweaver/commit/e3d14be5505e17183c07c00e6d1c939a22e04b54))
* **web:** add d3-force-3d layout worker and useForceLayout hook ([0e4e72e](https://github.com/Kehl-io/nestweaver/commit/0e4e72e97e697e30df82a093676b8f22e4893686))
* **web:** add edge gradients, keyboard graph navigation, and node drag ([8c7e4a5](https://github.com/Kehl-io/nestweaver/commit/8c7e4a54c6950dd809c2753b45e619d1029b59d0))
* **web:** add edge particles, accessible node list view, and view mode toggle ([34d9c99](https://github.com/Kehl-io/nestweaver/commit/34d9c99ab160f24876b79a26f43341e540da5a5c))
* **web:** add glassmorphism, navigation history, URL deep-linking, reduced effects toggle ([4018d75](https://github.com/Kehl-io/nestweaver/commit/4018d75e838c4dbe7ab6b9d4e9456dd4331387b5))
* **web:** add node labels for seed, selected, and hovered nodes ([8df07a4](https://github.com/Kehl-io/nestweaver/commit/8df07a42810f409112be295d5bbe51abba2ef966))
* **web:** add Obsidian-style graph polish — animated settling, always-visible labels, click-to-focus ([76e28f5](https://github.com/Kehl-io/nestweaver/commit/76e28f544b92f54f7a2655b90491b5523399f2cd))
* **web:** add R3F packages and graphDataSlice + useGraphBridge data layer ([0b298cd](https://github.com/Kehl-io/nestweaver/commit/0b298cd8feed3dc287ea45519eb069c2c3dcb8cc))
* **web:** add R3F renderer core — GraphCanvas, NodeInstanceMesh, EdgeInstanceMesh, GPU picking ([59403d3](https://github.com/Kehl-io/nestweaver/commit/59403d31a01f1195c91ab674c9145e1581e9d5a8))
* **web:** add visual effects — breathing, glow, bloom, hover interactions ([464e079](https://github.com/Kehl-io/nestweaver/commit/464e07917011ae69e5a4be92e6baeaa0404a7940))
* **web:** add WASM engine bridge, snapshot sync, and engine mode toggle ([7906319](https://github.com/Kehl-io/nestweaver/commit/790631988cdee53b6c39b3c8dd50902937ce6769))
* **web:** implement SemanticZoom camera bridge and CommunityOverlay with 3D hulls ([48141bb](https://github.com/Kehl-io/nestweaver/commit/48141bbc5e3aeb700a5244cc5b9ef1ff63555b25))
* **web:** migrate auxiliary components and remove Sigma.js dependency ([a9afa1c](https://github.com/Kehl-io/nestweaver/commit/a9afa1c8fc0afba22fe862a243dca97edbacd0cb))
* **web:** migrate GraphPanel and mode hooks from Sigma.js to R3F ([18fcc85](https://github.com/Kehl-io/nestweaver/commit/18fcc85c6a4226085710ea4d7332d96a71ddd134))
* **web:** wire GlassPanel into panels and add impact ripple on selection ([b755127](https://github.com/Kehl-io/nestweaver/commit/b75512781b3a634a4c4dbf717da5a58fb83a3746))
* **web:** wire WASM engine end-to-end, fix navigation history, fix GlassPanel cursor light ([cec096e](https://github.com/Kehl-io/nestweaver/commit/cec096eebe4622f5cfa3b634aa5412f162fcc374))


### Bug Fixes

* --force re-index idempotency + broken-links surfaces unresolved wikilinks (QA) ([bec3d12](https://github.com/Kehl-io/nestweaver/commit/bec3d128e35c52507ec509de9897e95521b1abf6))
* add --config to read-only commands, EdgeType PPR classification, parallel parsing with rayon ([3c3392d](https://github.com/Kehl-io/nestweaver/commit/3c3392dd371bcba63f20ea1c0655cbd3915cd102))
* add generation field to SSE events and update GPU picking docs ([fc26c87](https://github.com/Kehl-io/nestweaver/commit/fc26c87995670c934aa9d0c8ee09af4bfbd19f10))
* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address all code review low and nit findings ([03684dd](https://github.com/Kehl-io/nestweaver/commit/03684dd457b3d84ebf86d7618d84a48fa9ddef2e))
* address all remaining code review findings (14 fixes) ([a6e6777](https://github.com/Kehl-io/nestweaver/commit/a6e6777f0f1e262003667b5f0236e33d40bed6b6))
* address code review findings (12 fixes) ([b8c2e85](https://github.com/Kehl-io/nestweaver/commit/b8c2e857f1f22f9eb895716ae59e6000a694e150))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* affected-tests reaches Jest/Vitest tests + consistent not-found exit codes (QA) ([dd0d48c](https://github.com/Kehl-io/nestweaver/commit/dd0d48cb4b545dbf42d996520be694b24c2ba122))
* auto-populate Tantivy after brain add/refresh, fix remaining user issues ([7ab2872](https://github.com/Kehl-io/nestweaver/commit/7ab2872152a89806477a6641936a2a36745e410f))
* auto-populate Tantivy after brain add/refresh, implement wiki refresh timer, fix WAL flush ([2f1c168](https://github.com/Kehl-io/nestweaver/commit/2f1c16857103c707bb69980531e402621d14ff95))
* **bench:** add intent parameter to brain benchmark ([bbd2d18](https://github.com/Kehl-io/nestweaver/commit/bbd2d185eaa9b77cdb19a13170565c4de4c3518b))
* brain_status last_indexed, clusters exit code + adaptive resolution, get_summary diagnostics ([8e9540b](https://github.com/Kehl-io/nestweaver/commit/8e9540ba4ace42e23464071b9502d46b993356dc))
* **ci:** add --allow-multiple-definition for aarch64-linux release builds ([00902cc](https://github.com/Kehl-io/nestweaver/commit/00902cc098df0bd9794e657822264422116cbffd))
* **ci:** add build deps and lbug cache warming to release build jobs ([b82ca97](https://github.com/Kehl-io/nestweaver/commit/b82ca97ca58f5e73e491e0d07cc82427d3140a1c))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** add protobuf-compiler to CI, fix rustfmt formatting ([7d0c617](https://github.com/Kehl-io/nestweaver/commit/7d0c617f60f484251438c82e5631afa0910a3838))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** configure release-please to bump Cargo.toml workspace version automatically ([2d15106](https://github.com/Kehl-io/nestweaver/commit/2d15106fe3b9a52acaa7a59b3bb2ad803cd8d813))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** install g++ cross-compiler for aarch64-linux release builds ([19ac794](https://github.com/Kehl-io/nestweaver/commit/19ac794767aff49e08a4d4129d059ee9ed7ee3db))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** resolve clippy lints and add NESTWEAVER_NO_DAEMON to CI test/coverage jobs ([a257b88](https://github.com/Kehl-io/nestweaver/commit/a257b8880cdeac6989c7224795a719b94464f3e2))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **ci:** use LBUG_BUILD_FROM_SOURCE for release builds (reliable cross-platform) ([2bbf192](https://github.com/Kehl-io/nestweaver/commit/2bbf192af945d507cb2feedfbfd5c38deba1a424))
* **ci:** warm lbug cache for target architecture in release builds ([0c51629](https://github.com/Kehl-io/nestweaver/commit/0c51629063377141b06c6c1ebffd5de6f2c9a565))
* CLI alias resolver, SIGTERM handling, clusters exit code, brain status timestamps ([fb036eb](https://github.com/Kehl-io/nestweaver/commit/fb036eb340cce95d57978829b02a41176ebe4eca))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **cli:** collapse nested if to satisfy clippy::collapsible_if ([b59c3b4](https://github.com/Kehl-io/nestweaver/commit/b59c3b4252b4129c95b9588b4495b725fe0b416f))
* **client:** correct daemon spawn arg order and increase socket timeout to 5s ([745b46a](https://github.com/Kehl-io/nestweaver/commit/745b46a458a79b7d10302acbe2b256e75443bb4c))
* **client:** fix tonic UDS client connection for 0.13 ([6855634](https://github.com/Kehl-io/nestweaver/commit/685563433f8558e54fe3a34661980daa713901a9))
* **cli:** match --repo filter against repo display name, not just file path ([5db4215](https://github.com/Kehl-io/nestweaver/commit/5db421503d2e73c97481af12d4984fa75f2d408d))
* **cli:** restore --allow-writes flag on setup command ([d08e936](https://github.com/Kehl-io/nestweaver/commit/d08e9367445c6072c4bb6b2e997bdc5063a41e81))
* **cli:** thread --token-budget through project-context command ([d6a2136](https://github.com/Kehl-io/nestweaver/commit/d6a2136d492f3c0d6285396377e397bb0289d9d1))
* **cli:** write Claude Code MCP config to .mcp.json instead of .claude/settings.json ([2cc75cd](https://github.com/Kehl-io/nestweaver/commit/2cc75cd755eada04b9f0dcd2342918c576a92773))
* **daemon:** catch SIGTERM for graceful shutdown with socket/pidfile cleanup ([fb621a2](https://github.com/Kehl-io/nestweaver/commit/fb621a27ba75f02445dd9cbd6eb6c727ac9661c5))
* **daemon:** graceful shutdown via signal channel, open DB with write access, spawn_blocking for dispatch ([ccaf96c](https://github.com/Kehl-io/nestweaver/commit/ccaf96c86d1f8d0ebffb8c78e131555055d26683))
* **daemon:** use _with_store index variants to avoid DB lock re-acquisition ([1754e93](https://github.com/Kehl-io/nestweaver/commit/1754e9345fe93099efd437bf2b588d094826652e))
* **engine:** add timeout and EOF detection to MCP client to prevent wiki_sources hang ([63efb37](https://github.com/Kehl-io/nestweaver/commit/63efb372db8fe6e8dfe2249362b42e28a56c4a5b))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** boost project-scoped content in PPR ranking for project_context queries ([5bcebeb](https://github.com/Kehl-io/nestweaver/commit/5bcebebabcf0e6e6a420262ee4dd5380bf0281aa))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** correct interaction memory implementation per research alignment ([a4b4985](https://github.com/Kehl-io/nestweaver/commit/a4b4985698089c071e2b68a36264d38becd2849a))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** improve dead code detection accuracy with IMPORTS edge traversal and entry point coverage ([b7bd060](https://github.com/Kehl-io/nestweaver/commit/b7bd060b512fcbc4c51c8ab78ccf1daca62ef6c4))
* **engine:** limit cluster summaries to top-50 largest, skip singletons ([0a7f777](https://github.com/Kehl-io/nestweaver/commit/0a7f777dc2e866353e561c9d0ab65c656375e5b9))
* **engine:** reduce dead code false positives with type exclusion, dedup, and manifest entry points ([a2f5431](https://github.com/Kehl-io/nestweaver/commit/a2f5431701b250de1d071ba80160a3c0a8c82539))
* **engine:** robust MCP client with timeout, poisoning, reuse, and configurable timeout_secs ([18c04e8](https://github.com/Kehl-io/nestweaver/commit/18c04e8cdaf23b731039facc6a9f94975d73c778))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **grpc:** increase message size limit to 64MB for large responses (dead_code, clusters) ([3504d87](https://github.com/Kehl-io/nestweaver/commit/3504d87faed9e0df1472e3f7dcf42ab54e2d6cdf))
* hash-based instance ID to prevent socket path collisions and length overflow ([9639ee0](https://github.com/Kehl-io/nestweaver/commit/9639ee0dd9c89b62f2c64d8822f814a6db06df8e))
* hide deprecated --allow-writes flag from setup CLI help ([8c57598](https://github.com/Kehl-io/nestweaver/commit/8c5759818eb1f1d7d194b05230fb5f9882b9b4f7))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** correct project UID lookup in project_context tool ([93a8192](https://github.com/Kehl-io/nestweaver/commit/93a8192c8def0b91e3ab131fcb5a069b443005da))
* **mcp:** resolve ambiguous symbol names and repo display-name filter ([47196c1](https://github.com/Kehl-io/nestweaver/commit/47196c1257007b3bdfe33baa12c9bd39a5712a0f))
* **mcp:** resolve vault display names in vaults filter ([de25f50](https://github.com/Kehl-io/nestweaver/commit/de25f50699441a189ad74c3f1d01ae73e81df545))
* **mcp:** strip seeds array from default response to respect token budget ([6caf8f8](https://github.com/Kehl-io/nestweaver/commit/6caf8f885c705b9decc09e1a4511157719905387))
* **mcp:** tighten token budget with serialization-based final pass and relevance cost ([0f207b8](https://github.com/Kehl-io/nestweaver/commit/0f207b8e7c50e5880bab554d8b1e87ea82222fef))
* **mcp:** update tool count assertion for get_summary tool ([ede630a](https://github.com/Kehl-io/nestweaver/commit/ede630a48988aba66f22e028b9e111d13c331200))
* **mcp:** use ProjectContext intent as default for project_context tool ([65babbe](https://github.com/Kehl-io/nestweaver/commit/65babbef482852914b7b59cf1b7764436a66718f))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **parser:** improve entry point detection for React, Next.js, and frontend frameworks ([ef9dd13](https://github.com/Kehl-io/nestweaver/commit/ef9dd13f9a775dd126ccc513b66803bcd1f35428))
* **parser:** use TSX grammar for .tsx files to fix JSX parse failures ([042f6ee](https://github.com/Kehl-io/nestweaver/commit/042f6ee5d9e18722dec009bbe1627726f5245a59))
* regex line numbers + install-hook dry-run delta + multi-handler coverage (QA) ([c34bc1d](https://github.com/Kehl-io/nestweaver/commit/c34bc1d0737ecaa85ff8d36a0e2119110aaac465))
* remove duplicate PID write, fix brain_add_source for plain markdown dirs, implement daemon restart ([587698d](https://github.com/Kehl-io/nestweaver/commit/587698def37bd90951e600fc6c173c5a1631a573))
* remove stale --allow-mcp-add-sources references from tool descriptions and setup configs ([1038439](https://github.com/Kehl-io/nestweaver/commit/10384397515519eec03c0429d2066d42f0be3d2e))
* remove unused variables in autostart error path ([b36c0be](https://github.com/Kehl-io/nestweaver/commit/b36c0bea6cd13266f603072789481c518d5422cb))
* repo_display_name in materialize, --db on list-links, positional watch, --repo filter on impact ([efbaeee](https://github.com/Kehl-io/nestweaver/commit/efbaeeec9b3dcb660b22a14a0fea273b210bbcca))
* resolve db from --config on brain read commands (Bug [#19](https://github.com/Kehl-io/nestweaver/issues/19)) ([e25c00e](https://github.com/Kehl-io/nestweaver/commit/e25c00e9c53d08c5561485dd49cb275bb4d47613))
* restore project-context results, reject wiki fetch errors, restore CLI subcommands ([47031e1](https://github.com/Kehl-io/nestweaver/commit/47031e1c3328356c083273251acc791cae4e5b45))
* restore render_cost, add --config to all commands, bump version to 0.9.0 ([4ce7f3b](https://github.com/Kehl-io/nestweaver/commit/4ce7f3bd77f950b4252b8ab287b8c9873c54478e))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** add missing PROJECT rel tables and confidence column to schema ([fc2c55d](https://github.com/Kehl-io/nestweaver/commit/fc2c55ddd1865fd9ea6c3fd46a2b3c7dfd9069bf))
* **store:** add missing USES and ACCESSES rel tables to schema bootstrap ([6ca9bbf](https://github.com/Kehl-io/nestweaver/commit/6ca9bbf4da0cbe1e55e16fabfec6aaff6545fad7))
* **store:** add read-only Tantivy mode to prevent writer lock blocking searches ([1ecb7ec](https://github.com/Kehl-io/nestweaver/commit/1ecb7ec449ec15afd5497998bb0fcd5e470ac8db))
* **store:** deduplicate project nodes by name on upsert, correct MCP token budget measurement ([340a143](https://github.com/Kehl-io/nestweaver/commit/340a143bc77a6472244e464e416ca566bc1a4397))
* **store:** run schema migrations on open() for v0.7 DB compatibility ([fb5e300](https://github.com/Kehl-io/nestweaver/commit/fb5e3004942c248306212bed7572b4b13e9fab3f))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **store:** use SYMBOL_COLUMNS in update_symbol_embedding query ([607d0fa](https://github.com/Kehl-io/nestweaver/commit/607d0fabd6958e62e805afe52ff33eca6a41237a))
* surface project member notes in project_context (Bug [#12](https://github.com/Kehl-io/nestweaver/issues/12)) ([38c9414](https://github.com/Kehl-io/nestweaver/commit/38c941408eed4ceaa011613abddffb86ed9a6876))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* update stale daemonize references to daemonize2 in comments ([ebb6bee](https://github.com/Kehl-io/nestweaver/commit/ebb6bee738bb21bf69c31579e6e513ad46662bfc))
* watcher lock detection, daemon log rotation, actionable error messages ([6f0fd50](https://github.com/Kehl-io/nestweaver/commit/6f0fd503e7e46f1a511aa482eca1c8ae0db462a2))
* **watcher:** remove stale title→UID mapping on note rename before adding new title ([0c86af1](https://github.com/Kehl-io/nestweaver/commit/0c86af1488c6333341d332c553a7e813c9358e24))
* **web:** fix E2E test assertions and CI build order for frontend embedding ([a2f2269](https://github.com/Kehl-io/nestweaver/commit/a2f22699f6e09be9d0bb481b8be522a0bb187c61))
* **web:** fix edge rendering with depthTest disabled for visibility ([d4bc6d9](https://github.com/Kehl-io/nestweaver/commit/d4bc6d900ea6bcb47845a2d66cef4f33184adc37))
* **web:** replace LineSegments with instanced quads for visible edge rendering ([f036a6a](https://github.com/Kehl-io/nestweaver/commit/f036a6a61437b014500017d77de7e15d83eb91fb))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** run force layout to completion before rendering — no more flying nodes ([6cc0780](https://github.com/Kehl-io/nestweaver/commit/6cc07803039a00d6fe85a0aa5660a45817da67a5))
* **web:** strip Symbol/ prefix from kind in color lookup ([c488c28](https://github.com/Kehl-io/nestweaver/commit/c488c28281f1ffda15fa94c6e90a4874eb55503d))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))


### Performance Improvements

* **daemon:** skip Tantivy reindex after code repo indexing (Tantivy only indexes notes) ([b375e40](https://github.com/Kehl-io/nestweaver/commit/b375e4071d0f52fac34efe11a1e858c7312b604a))
* **engine:** tiered change detection for faster incremental indexing ([ca0f4f4](https://github.com/Kehl-io/nestweaver/commit/ca0f4f4d50ced1568b227e46e1c550da97a36273))
* **index:** defer PageRank computation to first query (lazy evaluation) ([b7b044c](https://github.com/Kehl-io/nestweaver/commit/b7b044c43f50dd351ed6e840b3ebfdab6e5c8323))
* **index:** parallelize markdown note parsing with rayon ([766f82e](https://github.com/Kehl-io/nestweaver/commit/766f82e5f82bfc76703a49db7857c69e2fc8f011))
* **pagerank:** add warm-start support for faster convergence after incremental updates ([c54a429](https://github.com/Kehl-io/nestweaver/commit/c54a4291e1d3d8de927200033e706a9226fb6713))
* **store:** bulk DETACH DELETE for file symbols instead of per-UID queries ([96b2fd3](https://github.com/Kehl-io/nestweaver/commit/96b2fd36d46d1336ea8d026ccb0dee296056ed53))
* **store:** wrap bulk index writes in transactions for 12x speedup ([0422b43](https://github.com/Kehl-io/nestweaver/commit/0422b43481c2ba3c82ba3518a01ab744d3275bcc))
* **tantivy:** update search index after daemon indexing operations ([da90564](https://github.com/Kehl-io/nestweaver/commit/da90564fa6a112629fb75d1e9815672dd7ad515f))
* **watcher:** bidirectional map for O(1) wikilink title lookup updates on rename ([e173612](https://github.com/Kehl-io/nestweaver/commit/e17361201a8cf9be0273affb96e0e14068e8a502))
* **watcher:** cache wikilink title lookup across batch, avoid per-note list_notes query ([5137d87](https://github.com/Kehl-io/nestweaver/commit/5137d871983375ba7f29bb60e2738aab19aace74))

## [0.11.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.10.0...nestweaver-v0.11.0) (2026-05-29)


### Features

* agent guidance — hard rules in guides + subagent hook (F14, F15) ([0f1e9a2](https://github.com/Kehl-io/nestweaver/commit/0f1e9a2de2106bba5a3d81852f7b81cca3879f8a))
* **algorithms:** add impact analysis BFS and substring search ([54164e0](https://github.com/Kehl-io/nestweaver/commit/54164e07e9fb9ce79345dedbd6ac6dd1ae2c9a40))
* **algorithms:** create nestweaver-algorithms crate with InMemoryGraph and PPR ([d3d2941](https://github.com/Kehl-io/nestweaver/commit/d3d294134ffa6ea0fa770c2d08562070267a6836))
* API contract graph — Contract nodes, IMPLEMENTS edges, drift (F2-core) ([70fd961](https://github.com/Kehl-io/nestweaver/commit/70fd9616929d5e124d0f95d82d5e681f4898518b))
* brain.* document-graph tools (F9) ([f095275](https://github.com/Kehl-io/nestweaver/commit/f0952759762f559836708d1e82c3b24db681be42))
* **engine:** add read_symbols — symbol-window source reads (F5) ([2759894](https://github.com/Kehl-io/nestweaver/commit/2759894df248da9c99c46d70dc35d8167e635a87))
* **engine:** affected_tests — static RTS for PR test selection (F13) ([0965cbd](https://github.com/Kehl-io/nestweaver/commit/0965cbde7db2d7962642a7c03fed6cc853373622))
* **engine:** finish agent feedback loop — TerminalSuccess + interactions show (F1) ([cfd0120](https://github.com/Kehl-io/nestweaver/commit/cfd0120d8ffbda88faea783ee1954de69c13d1a8))
* **engine:** inline high-confidence result bodies (F8) ([6543f72](https://github.com/Kehl-io/nestweaver/commit/6543f720737eb8522f984f6dd7dd1fd06807f706))
* **engine:** investigate bundle primitive (F10) ([03f765e](https://github.com/Kehl-io/nestweaver/commit/03f765ec5233bf3ac8f93a1049330d184ddf6c92))
* **engine:** lightweight result reranker (F17) ([55bc2f8](https://github.com/Kehl-io/nestweaver/commit/55bc2f8b1bca17301a214bcbd0ccc6b692470a43))
* **engine:** per-path dampen/boost ranking priors (F6) ([c183679](https://github.com/Kehl-io/nestweaver/commit/c183679cba79011dba4517a7a784a9363b2fc278))
* **engine:** retrieval-quality eval harness (P0.3) ([be5955f](https://github.com/Kehl-io/nestweaver/commit/be5955f86f99fea79bbceaf1e1a3e41b42ccd966))
* git-activity-dampened CodeRank (F12) ([61faf2d](https://github.com/Kehl-io/nestweaver/commit/61faf2d26e912d4c208082955ac4c35e3c34888a))
* memory-bank semantics — typed edges, lint, consolidate, related (F11) ([8d35b37](https://github.com/Kehl-io/nestweaver/commit/8d35b37d552223d3a7a17f8b125c6134a7c4523f))
* next-gen R3F web UI, algorithms/WASM crates, and v0.9.1 retrieval quality + eval harness ([028d62f](https://github.com/Kehl-io/nestweaver/commit/028d62f0eb5de29ad112c6a309995087143ce604))
* persisted graph_generation (P0.2) + ZSTD response cache (F16) ([283e352](https://github.com/Kehl-io/nestweaver/commit/283e352fa37b4f7aafc7d951ad989571f7174e0a))
* **schema:** add Symbol.end_line spanning a symbol's full source range (P0.1) ([45abd76](https://github.com/Kehl-io/nestweaver/commit/45abd764fe51a95b32ec41591fe509e87119e840))
* **store:** add graph_generation counter and watcher shared-store support ([d9bc01d](https://github.com/Kehl-io/nestweaver/commit/d9bc01d8416aa6fdd348a302cd2d31be865aac55))
* **store:** BM25 pseudo-relevance feedback (PRF) (F7) ([a950cac](https://github.com/Kehl-io/nestweaver/commit/a950cac348c8678d34de899eb99b7a79109d6391))
* **store:** trigram-accelerated regex_search + count_patterns (F3, F4) ([efaa376](https://github.com/Kehl-io/nestweaver/commit/efaa376ec7aad2bcda90af809ff6b86a7fe8761e))
* **wasm:** add nestweaver-wasm crate, msgpack export, and snapshot endpoint ([c94cd38](https://github.com/Kehl-io/nestweaver/commit/c94cd3842027181515026493cc6e14e6e1ae9447))
* **wasm:** build WASM binary and wire worker/bridge to real wasm-bindgen API ([afb466f](https://github.com/Kehl-io/nestweaver/commit/afb466fd2ed9b1ce860aa2b4d86b43c2d250bcf1))
* **web:** add --watch flag for live re-indexing and /api/v1/version endpoint ([e3d14be](https://github.com/Kehl-io/nestweaver/commit/e3d14be5505e17183c07c00e6d1c939a22e04b54))
* **web:** add d3-force-3d layout worker and useForceLayout hook ([0e4e72e](https://github.com/Kehl-io/nestweaver/commit/0e4e72e97e697e30df82a093676b8f22e4893686))
* **web:** add edge gradients, keyboard graph navigation, and node drag ([8c7e4a5](https://github.com/Kehl-io/nestweaver/commit/8c7e4a54c6950dd809c2753b45e619d1029b59d0))
* **web:** add edge particles, accessible node list view, and view mode toggle ([34d9c99](https://github.com/Kehl-io/nestweaver/commit/34d9c99ab160f24876b79a26f43341e540da5a5c))
* **web:** add glassmorphism, navigation history, URL deep-linking, reduced effects toggle ([4018d75](https://github.com/Kehl-io/nestweaver/commit/4018d75e838c4dbe7ab6b9d4e9456dd4331387b5))
* **web:** add node labels for seed, selected, and hovered nodes ([8df07a4](https://github.com/Kehl-io/nestweaver/commit/8df07a42810f409112be295d5bbe51abba2ef966))
* **web:** add Obsidian-style graph polish — animated settling, always-visible labels, click-to-focus ([76e28f5](https://github.com/Kehl-io/nestweaver/commit/76e28f544b92f54f7a2655b90491b5523399f2cd))
* **web:** add R3F packages and graphDataSlice + useGraphBridge data layer ([0b298cd](https://github.com/Kehl-io/nestweaver/commit/0b298cd8feed3dc287ea45519eb069c2c3dcb8cc))
* **web:** add R3F renderer core — GraphCanvas, NodeInstanceMesh, EdgeInstanceMesh, GPU picking ([59403d3](https://github.com/Kehl-io/nestweaver/commit/59403d31a01f1195c91ab674c9145e1581e9d5a8))
* **web:** add visual effects — breathing, glow, bloom, hover interactions ([464e079](https://github.com/Kehl-io/nestweaver/commit/464e07917011ae69e5a4be92e6baeaa0404a7940))
* **web:** add WASM engine bridge, snapshot sync, and engine mode toggle ([7906319](https://github.com/Kehl-io/nestweaver/commit/790631988cdee53b6c39b3c8dd50902937ce6769))
* **web:** implement SemanticZoom camera bridge and CommunityOverlay with 3D hulls ([48141bb](https://github.com/Kehl-io/nestweaver/commit/48141bbc5e3aeb700a5244cc5b9ef1ff63555b25))
* **web:** migrate auxiliary components and remove Sigma.js dependency ([a9afa1c](https://github.com/Kehl-io/nestweaver/commit/a9afa1c8fc0afba22fe862a243dca97edbacd0cb))
* **web:** migrate GraphPanel and mode hooks from Sigma.js to R3F ([18fcc85](https://github.com/Kehl-io/nestweaver/commit/18fcc85c6a4226085710ea4d7332d96a71ddd134))
* **web:** wire GlassPanel into panels and add impact ripple on selection ([b755127](https://github.com/Kehl-io/nestweaver/commit/b75512781b3a634a4c4dbf717da5a58fb83a3746))
* **web:** wire WASM engine end-to-end, fix navigation history, fix GlassPanel cursor light ([cec096e](https://github.com/Kehl-io/nestweaver/commit/cec096eebe4622f5cfa3b634aa5412f162fcc374))


### Bug Fixes

* --force re-index idempotency + broken-links surfaces unresolved wikilinks (QA) ([bec3d12](https://github.com/Kehl-io/nestweaver/commit/bec3d128e35c52507ec509de9897e95521b1abf6))
* add generation field to SSE events and update GPU picking docs ([fc26c87](https://github.com/Kehl-io/nestweaver/commit/fc26c87995670c934aa9d0c8ee09af4bfbd19f10))
* affected-tests reaches Jest/Vitest tests + consistent not-found exit codes (QA) ([dd0d48c](https://github.com/Kehl-io/nestweaver/commit/dd0d48cb4b545dbf42d996520be694b24c2ba122))
* regex line numbers + install-hook dry-run delta + multi-handler coverage (QA) ([c34bc1d](https://github.com/Kehl-io/nestweaver/commit/c34bc1d0737ecaa85ff8d36a0e2119110aaac465))
* resolve db from --config on brain read commands (Bug [#19](https://github.com/Kehl-io/nestweaver/issues/19)) ([e25c00e](https://github.com/Kehl-io/nestweaver/commit/e25c00e9c53d08c5561485dd49cb275bb4d47613))
* surface project member notes in project_context (Bug [#12](https://github.com/Kehl-io/nestweaver/issues/12)) ([38c9414](https://github.com/Kehl-io/nestweaver/commit/38c941408eed4ceaa011613abddffb86ed9a6876))
* **web:** fix edge rendering with depthTest disabled for visibility ([d4bc6d9](https://github.com/Kehl-io/nestweaver/commit/d4bc6d900ea6bcb47845a2d66cef4f33184adc37))
* **web:** replace LineSegments with instanced quads for visible edge rendering ([f036a6a](https://github.com/Kehl-io/nestweaver/commit/f036a6a61437b014500017d77de7e15d83eb91fb))
* **web:** run force layout to completion before rendering — no more flying nodes ([6cc0780](https://github.com/Kehl-io/nestweaver/commit/6cc07803039a00d6fe85a0aa5660a45817da67a5))
* **web:** strip Symbol/ prefix from kind in color lookup ([c488c28](https://github.com/Kehl-io/nestweaver/commit/c488c28281f1ffda15fa94c6e90a4874eb55503d))

## [0.10.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.9.0...nestweaver-v0.10.0) (2026-05-28)


### Features

* **cli:** show declared projects from --config alongside materialized ones in list-projects ([f336f93](https://github.com/Kehl-io/nestweaver/commit/f336f9314027fe5e35a89a67c12dd814c8e81e57))


### Bug Fixes

* add --config to read-only commands, EdgeType PPR classification, parallel parsing with rayon ([3c3392d](https://github.com/Kehl-io/nestweaver/commit/3c3392dd371bcba63f20ea1c0655cbd3915cd102))
* brain_status last_indexed, clusters exit code + adaptive resolution, get_summary diagnostics ([8e9540b](https://github.com/Kehl-io/nestweaver/commit/8e9540ba4ace42e23464071b9502d46b993356dc))
* **ci:** configure release-please to bump Cargo.toml workspace version automatically ([2d15106](https://github.com/Kehl-io/nestweaver/commit/2d15106fe3b9a52acaa7a59b3bb2ad803cd8d813))
* **engine:** add timeout and EOF detection to MCP client to prevent wiki_sources hang ([63efb37](https://github.com/Kehl-io/nestweaver/commit/63efb372db8fe6e8dfe2249362b42e28a56c4a5b))
* **engine:** robust MCP client with timeout, poisoning, reuse, and configurable timeout_secs ([18c04e8](https://github.com/Kehl-io/nestweaver/commit/18c04e8cdaf23b731039facc6a9f94975d73c778))
* **mcp:** tighten token budget with serialization-based final pass and relevance cost ([0f207b8](https://github.com/Kehl-io/nestweaver/commit/0f207b8e7c50e5880bab554d8b1e87ea82222fef))
* restore render_cost, add --config to all commands, bump version to 0.9.0 ([4ce7f3b](https://github.com/Kehl-io/nestweaver/commit/4ce7f3bd77f950b4252b8ab287b8c9873c54478e))
* **store:** deduplicate project nodes by name on upsert, correct MCP token budget measurement ([340a143](https://github.com/Kehl-io/nestweaver/commit/340a143bc77a6472244e464e416ca566bc1a4397))

## [0.9.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.8.0...nestweaver-v0.9.0) (2026-05-28)


### Features

* agent interaction memory — PPR bias from usage patterns ([3e568f6](https://github.com/Kehl-io/nestweaver/commit/3e568f638c09f6f2133731820bcd2048e1179e9b))
* **cli:** add --name flag for repo display name override ([52e1c05](https://github.com/Kehl-io/nestweaver/commit/52e1c05a6a20a1d529e27301e8ff2eb44d08be48))
* **engine:** add InteractionTracker with event recording, consolidation, and decay ([175b54c](https://github.com/Kehl-io/nestweaver/commit/175b54c530826d9fb03c3f51de4012e3d7e153ef))
* **engine:** expand brain_search queries with taxonomy aliases for better recall ([2eaa63b](https://github.com/Kehl-io/nestweaver/commit/2eaa63b9abc3a99399485b9362b2d9823558f434))
* **engine:** unify brain_search to return both vault notes and code symbols ([db07420](https://github.com/Kehl-io/nestweaver/commit/db07420e310ad86484531dc41e3bcfd9d10e954b))
* **mcp:** add interaction telemetry hooks to MCP tool dispatch ([207b4cc](https://github.com/Kehl-io/nestweaver/commit/207b4cc66c8de2c26b3dffc4f731f2aeb2f62cce))


### Bug Fixes

* **cli:** collapse nested if to satisfy clippy::collapsible_if ([b59c3b4](https://github.com/Kehl-io/nestweaver/commit/b59c3b4252b4129c95b9588b4495b725fe0b416f))
* **cli:** match --repo filter against repo display name, not just file path ([5db4215](https://github.com/Kehl-io/nestweaver/commit/5db421503d2e73c97481af12d4984fa75f2d408d))
* **engine:** correct interaction memory implementation per research alignment ([a4b4985](https://github.com/Kehl-io/nestweaver/commit/a4b4985698089c071e2b68a36264d38becd2849a))
* **engine:** limit cluster summaries to top-50 largest, skip singletons ([0a7f777](https://github.com/Kehl-io/nestweaver/commit/0a7f777dc2e866353e561c9d0ab65c656375e5b9))
* repo_display_name in materialize, --db on list-links, positional watch, --repo filter on impact ([efbaeee](https://github.com/Kehl-io/nestweaver/commit/efbaeeec9b3dcb660b22a14a0fea273b210bbcca))

## [0.8.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.7.0...nestweaver-v0.8.0) (2026-05-28)


### Features

* **cli:** add --allow-writes flag to setup command ([62d06e2](https://github.com/Kehl-io/nestweaver/commit/62d06e2587470d5ce7d7b2adfee1cfecdb60f9af))
* **cli:** add --tools flag for MCP tool allowlisting ([9d3c419](https://github.com/Kehl-io/nestweaver/commit/9d3c4191e7a7ae42065230a2e64be43e79907be3))
* **cli:** expose materialize-projects and detect-implicit-projects as subcommands ([1c03b29](https://github.com/Kehl-io/nestweaver/commit/1c03b29fe0aea75c62637cd3e8a33e56b5a6378f))
* **cli:** restore materialize-projects and detect-implicit-projects subcommands ([a86c787](https://github.com/Kehl-io/nestweaver/commit/a86c7877e5f211e4884bf251e879e578fde089e0))
* **engine:** add .brainignore support for vault indexing exclusion patterns ([9effc18](https://github.com/Kehl-io/nestweaver/commit/9effc18f741d557520232232e72db31de4f32cfd))
* **engine:** add HTML-to-markdown conversion for wiki content ingestion ([28b071f](https://github.com/Kehl-io/nestweaver/commit/28b071f3f366925b44a18cc21790821228d9be43))
* **mcp:** add intent to project_context, tool allowlist, and section title indexing ([36b238e](https://github.com/Kehl-io/nestweaver/commit/36b238e22d07b89b7f45f058d640792f315e90a4))
* **mcp:** wire intent through brain_context and cache get_summary in sidecar ([626918b](https://github.com/Kehl-io/nestweaver/commit/626918b10e8347d15c179b8f3c9f3ae8d6254194))
* **parser:** add JSX component edges and confidence-aware dead code BFS ([965e4c7](https://github.com/Kehl-io/nestweaver/commit/965e4c74bcef917e24a252b753b6e611263933fa))
* **parser:** enrich symbol extraction with constants, properties, types, and expanded queries ([201c27d](https://github.com/Kehl-io/nestweaver/commit/201c27d67b7a292f29e3fdbfc405f015518c2298))
* **resolver:** add file-level IMPORTS edges for every resolved import ([7ce74dc](https://github.com/Kehl-io/nestweaver/commit/7ce74dcfa7ee49296fc0554aedafd64963086fda))
* **resolver:** add monorepo workspace package and tsconfig path alias resolution ([2eff955](https://github.com/Kehl-io/nestweaver/commit/2eff955fe63bc7e1970f9f8bc1665de410a93a38))
* **resolver:** create IMPORTS edges from resolved import references ([5e65cf5](https://github.com/Kehl-io/nestweaver/commit/5e65cf5314617a465b8a2b451201a6f82c8ae4ec))
* **schema:** add USES and ACCESSES edge types with PPR weighting and workspace resolution ([8ab7c21](https://github.com/Kehl-io/nestweaver/commit/8ab7c21a57a00e2ad3870569b11d0f5efc212abb))
* **store:** add upsert helpers for idempotent project materialization ([1c24c4f](https://github.com/Kehl-io/nestweaver/commit/1c24c4ffaaab90a1f4c733003c26985d72b574dd))


### Bug Fixes

* address all code review low and nit findings ([03684dd](https://github.com/Kehl-io/nestweaver/commit/03684dd457b3d84ebf86d7618d84a48fa9ddef2e))
* auto-populate Tantivy after brain add/refresh, fix remaining user issues ([7ab2872](https://github.com/Kehl-io/nestweaver/commit/7ab2872152a89806477a6641936a2a36745e410f))
* auto-populate Tantivy after brain add/refresh, implement wiki refresh timer, fix WAL flush ([2f1c168](https://github.com/Kehl-io/nestweaver/commit/2f1c16857103c707bb69980531e402621d14ff95))
* **bench:** add intent parameter to brain benchmark ([bbd2d18](https://github.com/Kehl-io/nestweaver/commit/bbd2d185eaa9b77cdb19a13170565c4de4c3518b))
* CLI alias resolver, SIGTERM handling, clusters exit code, brain status timestamps ([fb036eb](https://github.com/Kehl-io/nestweaver/commit/fb036eb340cce95d57978829b02a41176ebe4eca))
* **cli:** restore --allow-writes flag on setup command ([d08e936](https://github.com/Kehl-io/nestweaver/commit/d08e9367445c6072c4bb6b2e997bdc5063a41e81))
* **cli:** thread --token-budget through project-context command ([d6a2136](https://github.com/Kehl-io/nestweaver/commit/d6a2136d492f3c0d6285396377e397bb0289d9d1))
* **engine:** boost project-scoped content in PPR ranking for project_context queries ([5bcebeb](https://github.com/Kehl-io/nestweaver/commit/5bcebebabcf0e6e6a420262ee4dd5380bf0281aa))
* **engine:** improve dead code detection accuracy with IMPORTS edge traversal and entry point coverage ([b7bd060](https://github.com/Kehl-io/nestweaver/commit/b7bd060b512fcbc4c51c8ab78ccf1daca62ef6c4))
* **engine:** reduce dead code false positives with type exclusion, dedup, and manifest entry points ([a2f5431](https://github.com/Kehl-io/nestweaver/commit/a2f5431701b250de1d071ba80160a3c0a8c82539))
* **mcp:** correct project UID lookup in project_context tool ([93a8192](https://github.com/Kehl-io/nestweaver/commit/93a8192c8def0b91e3ab131fcb5a069b443005da))
* **mcp:** strip seeds array from default response to respect token budget ([6caf8f8](https://github.com/Kehl-io/nestweaver/commit/6caf8f885c705b9decc09e1a4511157719905387))
* **mcp:** use ProjectContext intent as default for project_context tool ([65babbe](https://github.com/Kehl-io/nestweaver/commit/65babbef482852914b7b59cf1b7764436a66718f))
* **parser:** improve entry point detection for React, Next.js, and frontend frameworks ([ef9dd13](https://github.com/Kehl-io/nestweaver/commit/ef9dd13f9a775dd126ccc513b66803bcd1f35428))
* **parser:** use TSX grammar for .tsx files to fix JSX parse failures ([042f6ee](https://github.com/Kehl-io/nestweaver/commit/042f6ee5d9e18722dec009bbe1627726f5245a59))
* restore project-context results, reject wiki fetch errors, restore CLI subcommands ([47031e1](https://github.com/Kehl-io/nestweaver/commit/47031e1c3328356c083273251acc791cae4e5b45))
* **store:** add missing PROJECT rel tables and confidence column to schema ([fc2c55d](https://github.com/Kehl-io/nestweaver/commit/fc2c55ddd1865fd9ea6c3fd46a2b3c7dfd9069bf))
* **store:** add missing USES and ACCESSES rel tables to schema bootstrap ([6ca9bbf](https://github.com/Kehl-io/nestweaver/commit/6ca9bbf4da0cbe1e55e16fabfec6aaff6545fad7))
* **store:** add read-only Tantivy mode to prevent writer lock blocking searches ([1ecb7ec](https://github.com/Kehl-io/nestweaver/commit/1ecb7ec449ec15afd5497998bb0fcd5e470ac8db))
* **store:** run schema migrations on open() for v0.7 DB compatibility ([fb5e300](https://github.com/Kehl-io/nestweaver/commit/fb5e3004942c248306212bed7572b4b13e9fab3f))


### Performance Improvements

* **store:** wrap bulk index writes in transactions for 12x speedup ([0422b43](https://github.com/Kehl-io/nestweaver/commit/0422b43481c2ba3c82ba3518a01ab744d3275bcc))

## [0.7.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.6.0...nestweaver-v0.7.0) (2026-05-27)


### Features

* **engine:** add dead code detection via entry point reachability ([5f4221c](https://github.com/Kehl-io/nestweaver/commit/5f4221c81fa807f4a4ec6f6e8a321cf1a3e0b698))
* **engine:** add graph export to Cypher, GraphML, and Mermaid ([0a9e5f8](https://github.com/Kehl-io/nestweaver/commit/0a9e5f8e24b5b21795cb9a824f9f492a71267862))
* **engine:** add hierarchical code summaries for token-efficient retrieval ([9a26820](https://github.com/Kehl-io/nestweaver/commit/9a268209e2d31d424181646e33cf748bfac7e9b7))
* **engine:** add hub and bridge node detection ([30de53d](https://github.com/Kehl-io/nestweaver/commit/30de53d2c9be7da84c8d15e6a1d8e14051ecaa87))
* **engine:** add PR blast radius analysis with risk scoring ([70e2b91](https://github.com/Kehl-io/nestweaver/commit/70e2b9119e14444f1fda2bfac0b4857dd6eab65e))
* **engine:** add setup support for 10 additional AI tools ([1daf7d2](https://github.com/Kehl-io/nestweaver/commit/1daf7d213395894953efab33cea22eea9748d2e7))
* **engine:** generate SKILL.md conforming to Agent Skills standard ([a4c2e38](https://github.com/Kehl-io/nestweaver/commit/a4c2e38798698cb0fce8830d0a4cc62ff524bcb4))
* **mcp:** improve tool descriptions and add response_format parameter ([d178db1](https://github.com/Kehl-io/nestweaver/commit/d178db1aed20d6d29684d2e7eb0a4f48977ce691))
* **parser:** add Julia, SQL, HCL, Fortran, and Pascal language support ([9638978](https://github.com/Kehl-io/nestweaver/commit/9638978fef35ca8797cbfb1fe3c5a63351ab80d2))
* **parser:** add Lua, Bash, Scala, and Elixir language support ([d3556c1](https://github.com/Kehl-io/nestweaver/commit/d3556c1cfec308d64c4f6d3772d8b209ebb50ae7))
* **parser:** add Vue, Svelte, Astro, and SystemVerilog language support ([97509e3](https://github.com/Kehl-io/nestweaver/commit/97509e37d72f48b7666079689d2a84ecb6851355))
* **parser:** add Zig, Objective-C, Groovy, and PowerShell language support ([d4472c5](https://github.com/Kehl-io/nestweaver/commit/d4472c584b81b84528eca76d54396b328cafb8ad))
* **store:** dynamic PPR with query-type-aware alpha and edge weighting ([55123a3](https://github.com/Kehl-io/nestweaver/commit/55123a3d062c3eb091a81fca716e300d14587877))


### Bug Fixes

* address all remaining code review findings (14 fixes) ([a6e6777](https://github.com/Kehl-io/nestweaver/commit/a6e6777f0f1e262003667b5f0236e33d40bed6b6))
* address code review findings (12 fixes) ([b8c2e85](https://github.com/Kehl-io/nestweaver/commit/b8c2e857f1f22f9eb895716ae59e6000a694e150))
* **mcp:** update tool count assertion for get_summary tool ([ede630a](https://github.com/Kehl-io/nestweaver/commit/ede630a48988aba66f22e028b9e111d13c331200))

## [0.6.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.5.1...nestweaver-v0.6.0) (2026-05-27)


### Features

* **cli:** add --stats flag and output control flags ([52449fb](https://github.com/Kehl-io/nestweaver/commit/52449fb46b96360e7a7cab9b97dff43c5a9f2f25))
* **engine:** add watch mode for live code re-indexing ([13e2fba](https://github.com/Kehl-io/nestweaver/commit/13e2fba37bb6c93743c87f02992bd2dd537a1d07))


### Bug Fixes

* **ci:** add --allow-multiple-definition for aarch64-linux release builds ([00902cc](https://github.com/Kehl-io/nestweaver/commit/00902cc098df0bd9794e657822264422116cbffd))


### Performance Improvements

* **engine:** tiered change detection for faster incremental indexing ([ca0f4f4](https://github.com/Kehl-io/nestweaver/commit/ca0f4f4d50ced1568b227e46e1c550da97a36273))

## [0.5.1](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.5.0...nestweaver-v0.5.1) (2026-05-27)


### Bug Fixes

* **ci:** install g++ cross-compiler for aarch64-linux release builds ([19ac794](https://github.com/Kehl-io/nestweaver/commit/19ac794767aff49e08a4d4129d059ee9ed7ee3db))

## [0.5.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.4.0...nestweaver-v0.5.0) (2026-05-27)


### Features

* **cli:** add shell completions, miette diagnostics, and zero-config index ([e483aaf](https://github.com/Kehl-io/nestweaver/commit/e483aaf735dad9875ce77b7e5da167671deed0d3))
* **engine:** add progress bars to indexing pipeline ([e328690](https://github.com/Kehl-io/nestweaver/commit/e328690df1de77f1286d4db710ab0cc189fae4c6))


### Bug Fixes

* **ci:** use LBUG_BUILD_FROM_SOURCE for release builds (reliable cross-platform) ([2bbf192](https://github.com/Kehl-io/nestweaver/commit/2bbf192af945d507cb2feedfbfd5c38deba1a424))

## [0.4.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.3.0...nestweaver-v0.4.0) (2026-05-27)


### Features

* **cli:** add standard output control flags ([60b218a](https://github.com/Kehl-io/nestweaver/commit/60b218ad46ce414934836aad8b9c52bd78939dfd))


### Bug Fixes

* **ci:** warm lbug cache for target architecture in release builds ([0c51629](https://github.com/Kehl-io/nestweaver/commit/0c51629063377141b06c6c1ebffd5de6f2c9a565))

## [0.3.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.2.0...nestweaver-v0.3.0) (2026-05-27)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))


### Bug Fixes

* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* **ci:** add build deps and lbug cache warming to release build jobs ([b82ca97](https://github.com/Kehl-io/nestweaver/commit/b82ca97ca58f5e73e491e0d07cc82427d3140a1c))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* **web:** fix E2E test assertions and CI build order for frontend embedding ([a2f2269](https://github.com/Kehl-io/nestweaver/commit/a2f22699f6e09be9d0bb481b8be522a0bb187c61))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))

## [0.2.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.1.0...nestweaver-v0.2.0) (2026-05-27)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))


### Bug Fixes

* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))

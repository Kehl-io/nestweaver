# PR #389 user-feedback validation

Status: follow-up fixes reviewed and locally validated; exact PR-head CI remains
the merge acceptance gate. This record supersedes the initial signoff for the
findings below.
The reviewed baseline is `d7bc6583b7da8ccbfbfe6a86851f3df814f363ac`.
All two critical findings, ten suggestions and three nits identify valid gaps.

| Finding | Validation and correction |
| --- | --- |
| Concurrent journal cancellation can be overwritten | Read/check/rename was not exclusive. A separate per-operation stable journal lock serializes checkpoints without requiring the long-lived publication root lock. Selection and discard use the same journal authority. A paused-checkpoint regression makes concurrent cancellation fail rather than falsely commit and disappear; retry preserves cancellation. |
| Watcher displacement/stop can return before draining | Registration and task publication were separate from stop. A lifecycle mutex now serializes publication and stop snapshots; retained predecessor handles keep displaced and cancelled-stop work visible until finished. Replacement workers wait for predecessors before indexing. Barrier regressions cover unpublished tasks, force/orphan displacement and cancelled stop. |
| Ambiguous impact JSON differs | One shared builder now supplies the ambiguity note and canonical UID/name/file/line candidates consistently. Manual comparison also exposed full-Symbol versus compact-candidate drift, corrected by the builder. The CLI's existing repository-filter-specific remedy delegates to the same helper. MCP has no `repo` filter argument, so its unfiltered response uses the unfiltered remedy. |
| Writer authority survives anchor substitution | `DbWriteLease::authorizes` now checks its retained anchor. Writers derived from a namespace guard retain the same locked descriptors; substitution revokes both writer and creation authority. |
| CURRENT mutators can omit root authority | CURRENT compare-and-swap and operation selection require and validate `PublicationRootLock`. Unrelated or replaced root authority is rejected before journal/CURRENT mutation. |
| Config-only exclusions leave stale rows | Eligibility reconciliation runs before the unchanged-SHA shortcut. Excluded modifications retract both File and Symbol nodes. A stored eligibility fingerprint invalidates the SHA shortcut when excludes, unskip or source-size policy changes; absent legacy fingerprints reconcile once. Server reconciliation uses its normal publication transaction. Live daemon index/watch admission reloads the bound configuration policy, refusing invalid edits before mutation. Re-admission, bare Git size limits and rename boundaries are covered by regressions. |
| Explicit setup executes arbitrary preserved commands | Confirmed with the old CLI and a harmless marker-producing repository wrapper. Probe execution now requires the canonical running NestWeaver executable, a bounded MCP argument grammar, no configured environment overrides and an existing nonempty database. Unsupported custom registrations remain preserved but unexecuted. |
| Traversal resets the caller deadline | The caller's instant is passed into traversal as well as the database deadline guard. An already-expired traversal regression prevents restarting a fresh 60-second budget. |
| Project token budget omits final fields | Budget enforcement measures the actual response after provenance and honesty fields. Exact-boundary handling accounts for the serialized size change between boolean values. |
| Restricted detect-changes omits contract fields | Restricted responses retain deadline/work-budget, traversal/timing and count-relation fields. Unavailable process analysis reports a lower-bound relation instead of an exact zero. |
| Ruleset self-test uses only synthetic input | CI feeds the committed ruleset to the verifier; the self-test derives its baseline from that file before testing counterexamples. Ruleset enforcement, main selection and no-bypass/deletion/force-push protection are checked too. |
| Duplicate lockfile in git | Removed the byte-identical evidence copy. Its source commit and SHA-256 remain in `lockfile-provenance.json`; the original CI artifact retains the copy. |
| Ambiguous impact comment contradicts normalized arrays | The comment now describes candidates plus empty impact aliases, distinguished by `status: ambiguous`. |
| Retrieval gate spelling is wrong | Documentation uses the serialized `degraded-unknown` value. |
| Exclusion log says minified/generated | The watcher log identifies configured exclusion policy separately. |

## Evidence and limits

The old-binary setup reproduction used SHA-256
`1056e20324823ec54c420a9708bc0458b6bdf51c8f65683d092d745fd2a543ce`,
matching the retained manual-regression provenance for the baseline. Setup exited
zero and the repository wrapper wrote its marker. The fixture was disposable;
no production registration or database was involved.

The existing release and performance artifacts describe their recorded earlier
source commits. They do not prove these follow-up changes. Native regressions,
independent review, manual replay and fresh final-head CI are recorded below
when completed.

## Completed local validation

- Publication-operation journal suite: 17 passed; publication suite: 39 passed;
  writer-authority suite: 24 passed, including substitution of derived guards.
- Final rebuilt MCP suite: 339 passed, including final-response budget boundaries, provenance,
  restricted detect-changes contracts and deadline behavior. Final schema suite: 86 passed.
- Setup registration suite: 28 passed; bounded probe suite: 5 passed.
- Full store suite: 566 passed, two intentionally ignored.
- Targeted eligibility tests passed for config exclusions, unskip re-admission,
  and transactional policy metadata migration/rollback/reopen.
- Both deterministic watcher regressions passed: force/orphan displacement with
  cancelled stop, and stop racing registration before task publication.
- The rebuilt CLI refused the same marker wrapper executed by the baseline.
  Canonical absolute and PATH-resolved NestWeaver registrations both passed
  initialize + tools/list with all six lite tools. Host activation remains
  explicitly unverified by this server-side handshake.
- Committed-ruleset validation and self-test passed; file-based counterexamples
  rejected disabled enforcement, bypass, wrong branch and wrong check app.
  Workflow lint passed.

The real CLI live-daemon regression passed exclusion and re-admission at the
same Git SHA and daemon PID, with current status inventory and direct-RPC
rejection of malformed TOML and invalid globs. The duplicate-URL/different-root
status regression also passed. A running watcher retains its registration-time
policy; index and new watcher admissions adopt edited eligibility.

The final manual ambiguity replay produced identical complete direct/daemon
JSON envelopes and exit code 3 on both paths. Canonical setup handshakes and
wrapper refusal were repeated with that final binary; retained
[manual provenance](evidence/pr-389-feedback/manual-provenance.json) binds the
results to the binary and source-file hashes.

The broad engine run recorded 1,520 passes, two subprocess-launch failures and
four intentional ignores. The two tests launch their current executable; a
concurrent rebuild had replaced that executable during this local run. After
compilation stopped, the complete 39-test publication suite passed, including
both failed subprocess cases. This is a failed broad run followed by a successful
focused rerun, not a claim that the original run was green. Final PR CI reruns
the full suite without that local build/test interference.

Workspace compilation, formatting, workflow lint and committed ruleset checks
passed. Independent review and the local regression gate are signed off. The PR
checks must be green for the actual final head before merge; earlier green runs
are not substituted for that gate.


## CI fixture correction

CI run [34766259109](https://github.com/Kehl-io/nestweaver/actions/runs/34766259109)
failed the macOS status-path test because its fixture wrote only `instance_id`,
not a valid instance configuration. Fresh eligibility reload correctly rejected
that file. The same failure was reproduced locally on Linux. The fixture now
uses the existing complete-config helper and retains its canonical-path assertion;
production behavior is unchanged.

That macOS run passed its engine (1,520), MCP (339) and store (566) tests; its
daemon suite had 395 passes and this one failure. These are partial results from
a failed CI job. The manual binary/source provenance explicitly identifies
`2edadc77`; the following correction repaired that test fixture without changing runtime
behavior. The later path-alias correction is recorded separately below. A new CI run must validate the corrected PR head.

The corrected fixture was rebuilt with the workspace feature set; all 384 local
daemon tests passed, including the canonical-path test and watcher lifecycle
regressions. Formatting and diff checks passed.


## Local repository aliases

The next CI run, [34767853323](https://github.com/Kehl-io/nestweaver/actions/runs/34767853323),
passed all 396 macOS daemon unit tests, then exposed a real integration failure:
a config naming a `/var` repository did not match the daemon's canonical
`/private/var` spelling. Exclusion lookup compared only strings, so a config-only
exclude could still be ignored through a local alias. A Linux CLI replay with a
symlinked repository reproduced the retained symbol after exclusion.

`exclude` and `unskip` now share one lookup that preserves lexical remote/path
matching and recognizes canonical aliases of existing local paths. The live
same-PID/same-SHA regression deliberately names a symlink on every Unix platform;
a separate matrix checks both policy halves through real, alias and canonical
paths and refuses unrelated roots. This fixes runtime behavior rather than
canonicalizing the fixture to hide the failure.

The CLI alias replay now observes `ok` → `not_found` → `ok` across initial index,
exclusion and re-admission. [Alias replay provenance](evidence/pr-389-feedback/alias-provenance.json)
records the baseline/candidate binary hashes and changed source hashes. All 70
configuration tests passed.

The rebuilt MCP suite passed all 339 tests. The focused live-daemon regression
passed with the explicit symlink fixture, unchanged SHA/PID, fresh inventory,
re-admission and invalid-config refusal. The broader daemon integration suite
remains part of the final CI gate.

## Final two suggestions

Reviewed against `4334776c1f2da3171ae42b4d8ad4f7e32ad86f56` (whose
[CI run passed](https://github.com/Kehl-io/nestweaver/actions/runs/34769866023)).
Both suggestions are valid:

- `finalize_project_budget` had no outer termination bound. A response with one
  title `x`, budget 44, and the existing metadata-overrun note repeats the
  true/false size cycle: the stored count is 45 while the final JSON measures
  44. Replacing that note with itself cannot resolve it. The loop now allows
  only the row-removal passes plus a final measurement; exhausted accounting
  returns an error rather than unmeasured success. Boundary handling grows an
  existing note so this case returns a correctly measured response, and a
  repeated finalization is stable.
- Both filesystem and bare-reader incremental rename arms removed destination
  symbols but retained a destination File when preparation returned
  `PolicySkipped`. Both now delete the destination File in the same transaction
  before recording the skip. The regression seeds stale destination coverage,
  performs a Git rename to `app.min.js`, and explicitly asserts that the
  incremental rename arm ran. Both routes failed before the correction with
  the stale File still present; the unaffected `keep.js` remains the control.

After correction, all 11 rename tests and all 10 policy tests passed, including
both previously failing destination-File cases and the watcher rename control.
All 340 MCP tests passed. The budget boundary tests cover the existing-note
cycle, exact final token counts, and idempotent repeated finalization. Fresh full-suite/CI validation is
recorded in the PR description; the prior green run above is baseline evidence,
not validation of the new commit.

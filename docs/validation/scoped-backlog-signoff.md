# Validation of the 18 scoped backlog fixes

Status: reviewed, validated and signed off by the orchestrator on 2026-09-13
for opening the implementation PR. Final PR-head CI remains a separate gate.

The integration branch starts at main `5149d22cd885107fa5d8f047a00d87e1fff86847`.
The successful release/CI validation candidate is `f4a1bc3ddb0b7cdd71b9becff8aadc25a8b7f9e1`.
A subsequent four-line logging-filter correction is `488c5f8d`; it preserves INFO
by default and enables the diagnostic events used by the instrumented replay.
Later evidence-only documentation must not be described as an independently
built source candidate. This record covers the 18 scoped backlog items, including
two measurement items; it does not claim deferred parent mitigations were shipped.

## Review and native findings

Lifecycle, persistence and interfaces were delegated separately, integrated on
one branch and reviewed again. Native validation identified additional defects:
incremental and dependent edge parsing bypassed excludes; failed Project topology
COPY could leave native adjacency storage unreadable; the watcher fixture checked
for a drained writer before publication finished; direct impact omitted provenance.
The fixes retain the original failure/reopen, displacement and parity assertions.

Other fixes address unsafe automatic execution of preserved setup commands,
replaceable temporary authority storage, accidental database creation during
runtime pruning, retained staged seed links, swallowed database deadlines, and
release-fault checks that could mistake an unrelated build failure for proof.

The first complete non-daemon workspace run recorded 4,731 passes, six failures
and eight ignored tests across 59 targets. Its six failures were three help/registry
contracts, one older not-found shape assertion, direct/daemon impact provenance,
and an invalid nullable semantic-diagnostic comparison. It is a failed run, not
acceptance. Rebuilt help contracts (30), daemon root checks (57), CLI impact,
byte-for-byte direct/daemon impact parity and truncation-key parity subsequently
passed. The complementary daemon run passed all 196 tests, including 56 process tests.
Fresh CI Clippy passed on f4a1bc3d, so the duplicate local native Clippy build was
interrupted rather than counted as a pass. Fresh f4a1bc3d E2E and macOS Metal jobs passed. The rebuilt MCP suite passed all 338 tests with both daemon and embed features.
All required jobs in [fresh CI](https://github.com/Kehl-io/nestweaver/actions/runs/34738345322)
and the [normal release dry-run](https://github.com/Kehl-io/nestweaver/actions/runs/34738154634)
completed successfully.

Independent final evidence review verified the retained checksummed files.
Fault and performance proofs are from `a771754e`; signed macOS and normal release
validation use `f4a1bc3d`; instrumented local replay uses `488c5f8d`. Both release
fault verification/cleanup jobs passed, with canaries independently confirmed
closed unmerged and their exact branches absent. The injected-failure run is
intentionally failed overall. Final review found no additional implementation
or evidence blocker before opening the PR. The orchestrator verified the subsequent normal-release
proof: four successful unique targets, four staged artifacts, bundle verifier
exit zero, unchanged external visibility, and canary #388 closed unmerged with
its exact branch independently confirmed absent.

## Scope evidence map

| Item | Implementation and review basis | Acceptance evidence |
| --- | --- | --- |
| Exact-SHA release enforcement | Applied main ruleset, strict Required CI, exact run/SHA/artifact inventory, fail/omit proof validation | Policy selftests and six counterexamples pass; real automation PR #385 blocked with no checks; fail/omit and normal exercises prove exact enforcement, artifact completeness and cleanup |
| UI supervisor error cleanup | Cleanup precedes every terminal supervision error | Three real tonic-peer cleanup regressions pass |
| Corrupt git activity | Versioned payload corruption rejected; legacy distinction preserved | Corrupt/archive and legacy/v2 round-trip regressions pass |
| Hook idempotency | Recognize unchanged installed hooks before irrelevant foreign-container validation | Targeted hook and engine suite pass |
| Canonical query JSON | Shared impact envelope, provenance preservation, context/status disclosures | CLI/MCP fixture, rebuilt transport parity and all 338 MCP tests pass |
| Planned publication creation | Durable staged ownership before target exposure, repeated crash recovery and seed retirement | Real CLI repeated-crash/reopen and ownership/refusal regressions pass |
| Linux archive baseline | Extracted binary and bundled runtimes checked against GLIBC 2.35, real negative ELF fixture | Earlier ARM archive passes; both f4a1bc3d Linux archive targets and complete normal dry-run proof pass |
| Missing daemon socket | Verified live writer reported as running but unreachable | Real missing-socket process regression passes |
| Watcher lifecycle | Session identities, conditional stop, displacement and drain semantics | Portable event/displacement/drain matrix passes locally; ad-hoc signed macOS fixture passes with all 63 daemon process tests |
| Explicit setup MCP probe | Stored command/args/env initialize and tools/list handshake; bounded output/time; unsupported host context disclosed | Nine setup tests and real candidate lite/full handshakes pass |
| Large detect-changes | Independent traversal work cap, cooperative DB/engine deadline, honest partial gate and uncached timeout | Native timeout/recovery and 56-file work-budget regression pass; four full-source instrumented requests pass with exact totals and responsive concurrent status |
| Linux supervision | PID-generation-checked cgroup provenance and unknown fallback | Cgroup fixtures and rebuilt status rendering pass |
| Production-degree remove-repo | Two isolated complete degree ladders with preselected repeat/decision criteria | Two ladders pass repeatability; no further scaling mitigation justified by fixture |
| Restore namespace anchors | Persistent account registry, stable identities, admitted directory handles and compatibility locks | Replacement/exclusion and registry revocation regressions pass |
| Publication root anchors | Same trusted authority with staged identity and repeated interruption protection | Root replacement and staged publication recovery regressions pass |
| Per-repository exclusions | One compiled policy for reader, incremental preparation, dependent resolver and watcher; honest inventory | 64 interface engine regressions and complete engine suite pass |
| Materialization bulk correctness/performance | Quoted note/symbol COPY; prepared topology inserts; exact four-stage rollback/reopen; matched synthetic benchmark | Quoted endpoints and unchanged four-stage matrix pass; paired performance passes at 339.494x / 345.091x, leases below 0.45 seconds |
| Unidentified runtime remediation | Exact owned directory inspection; matching existing DB lease; inode revalidation; admitted nonrecursive cleanup | Nine lifecycle/remediation regressions pass, including refusal cases |

## Evidence limits

The authority registry protects cooperating upgraded writers while that trusted
registry is intact. It does not claim resistance to arbitrary same-UID tampering
or direct database writes. Existing compatibility locks remain, but older clients
do not gain the upgraded path-replacement guarantee.

Setup handshakes prove the stored local invocation can initialize and list tools;
they do not prove Cursor, Codex or Claude Code activated that configuration.
Automatic first-index setup does not execute a preserved custom command.

The detect-changes deadline is cooperative database/engine coverage, not a hard
end-to-end latency guarantee. Engine phase timings, MCP symbol sorting and daemon
serialization are distinct measurements. The historical 300-second timeout was
not reproduced by the installed control; the 56-file fixture is a documented
reconstruction, not the unavailable exact historical graph.

Performance uses the pinned current engine and deterministic synthetic data.
The materialization reference reproduces the historical insertion algorithm on
139,509 memberships, not the unavailable production dataset or its reported
21-minute baseline. Remove-repo decisions use removal-only means, not Criterion
measurements that include teardown. Whole-invocation RSS is not per-degree or
per-algorithm memory. See [materialization](project-materialization.md) and
[remove-repo criteria](remove-repo.md). macOS watcher signing is ad-hoc, not
Developer ID distribution signing.

See [large-request evidence](detect-changes.md) for fixture provenance, phase
timings, exact completeness and the observed limits.

## Signoff and final PR gate

All 18 scoped items have been implemented, independently reviewed and validated.
Review findings were fixed before this signoff. The orchestrator approves opening
the implementation PR based on the evidence above. No merge, release or installed
version update is claimed.

The implementation PR must pass Required CI on its final head before handoff.
Evidence/documentation commits do not retroactively change the source SHAs of
recorded release, benchmark or local replay artifacts.

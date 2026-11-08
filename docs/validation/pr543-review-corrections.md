# PR543 independent review corrections

This ledger records corrections to three P2 findings on `15b27f01` in the
existing integration PR. The original [41-item ledger](retrieval-small-backlog.md)
and [two-fix follow-on ledger](diagnostic-and-compare-export.md) retain their
historical source/artifact qualifications. These are corrections within the
same 43-package scope, not additional backlog closures or a release.

## Causal baseline

Baseline standard Debug binary SHA256:
`0cc53c872285b70b46e2efbcbce8bed27a5ef389f2bf1c5e844a8b8a9c222304`.
Native 0.22.1/CMake Debug, parsed-cache 16 and resolver 22 remain unchanged.
Debug RustEmbed reads assets from disk; UI qualification binds actual served
index/JS/CSS and complete manifests separately from the binary.

1. **CLI ambiguity disclosure:** three real notes (two Same and one Unique)
   produced useful MCP results with Same's candidates, while both CLI JSON and
   text returned0 without Same or its ambiguity. Typed narrowing and shared
   renderers discarded the new field; the both-connected federation merger has
   the same reconstruction gap. The latter sibling was source-confirmed during
   RCA and needs its targeted tests/replay.
2. **Missing bridge debt:** an actual recent daemon relinker pass settled with
   Keep's note/section code edges2 and clean debt. Policy-only exclusion then
   reached Current/Keep-only inventory while edges remained0/pending=false over
   a bounded12s observation. Ordinary full refresh recorded pending debt and
   restored the exact two links. No retained-note save, restart, synthetic state
   or hourly wait supplied recovery. Owned daemon exit0/protected unchanged.
3. **Folder clipping:** two new tests against the actual old bundle failed at
   final-row containment, after real64-file/precondition checks. No-folder final
   row852..872 was visible inside clipping bottom876; selected-folder last
   row937..957 and retained-alert last row1017..1037 were clipped at maximum
   normal wheel scroll. Ancestor scroll stayed0. Both were intended causal
   failures, not setup failures. UI/daemon cleanup0/protected unchanged.

The baseline UI was `index-tRvg4vEe.js` SHA256
`d23f8e5d7e01c3843727036414a1d975c89e9162366f56f90ac1ac9c2f3c9411`,
index SHA256
`f4e6ab4ea09558cd8322c626a5989a71b7c0696b574610f115058246768baa32`.
Actual HTTP assets and all32 dist files matched before/after the red run.

## Scoped implementation

**Disclosure:** one schema-level Value-backed contract normalizes producer,
shared CLI capture/rendering and both-connected federation diagnostics. It
preserves useful ranked success and original source-scoped candidate handles,
separately from genuine unresolved inputs. Bounds are256 diagnoses per tier,
50 candidates,4096 UTF-8 bytes per indivisible textual leaf and20,000 escaped
serialized disclosure bytes including metadata. Whole-entry/candidate omission
is explicit; handles and paired UID arrays are not sliced. Prior producer cuts
are carried without double counting, and malformed known fields produce a
bounded non-echo marker. MCP presentation cuts preserve atomic ambiguity data.
Existing normal all-ambiguous/miss exits remain3/2. Pre-existing feature-mode
all-ambiguous and generic federation no-connected disposition remain outside
this correction.

**Bridge repair:** ordinary IndexVault, automatic policy replacement and legacy
derivation call one daemon-private full-refresh operation. It records code-link
debt after the returned attempt, before Result propagation, including failures
after commit. It uses the existing consumer `state.db_path` namespace, adds no
nested gate, preserves the original Result and removes the former standalone
ordinary marker. Caller locking/stamping and global scheduling remain intact.
Existing best-effort sidecar persistence/startup recovery are not upgraded to
atomic graph/debt durability by this fix.

**Files:** the loaded wrapper is a constrained flex column; header/alert retain
natural height; the scrolling tree flexes into remaining height with a zero
minimum. Selection, keyboard, ARIA and request ownership are unchanged. A focused
64-file fixture leaves normal data untouched. The required E2E step runs both
normal and focused acceptance; an offline shell-dispatch contract proves either
failure fails the step. New tests never runtime-skip or inflate deadlines.

## Review and artifact preparation

Independent source reviewers and a separate judge approved all three lanes.
Review found and corrected malformed prior-cut mutation, malformed known-field
echo, additive omission/idempotence and public-stamp retention cases before the
shared build. Rejected unreachable finalizer concerns did not trigger changes.
Source approval alone does not establish compiled/runtime acceptance.

The single frontend TypeScript/Vite production build passed. Candidate JS:
`index-DRV5vBB1.js` SHA256
`8a719eaa982dfbe138dc11b0457ba82906e24d166cb51d650e84580f5fa905b4`;
index SHA256
`ad997035680a3499a3c7830794cbf2c8cdcc5ebe9e0e473b47db4ed89975ada5`.
All32 generated files are frozen for candidate acceptance. The full offline CI
runtime-contract suite passed 29 tests, including normal/focused success and both
failure-propagation controls. Native dependencies, feature shape and repository
linker settings were not changed.

## First compiled checkpoint and test-fixture correction

The first cached workspace compile passed and froze intermediate binary SHA256
`c063da34e18cf77776dee9eae7bddd239935f49d30f0cff36cb9a0bb9136feef`.
Schema 121, federation 249, root 539, client 130 and MCP 608 tests passed. The daemon
invocation reported 548 passed/one failed/one ignored. Its new failure-path fixture
incorrectly expected an oversized note to block publication; oversized files are
legitimate bound-policy skips, so Current was correct. The actual committed-error
and repair half of that test had already completed successfully.

The owner and independent judge replaced that assumption with a deterministic
test-only policy race after a verified real commit and new debt mark. A valid
comment changes the raw policy digest without changing eligible notes, causing
the real postflight identity check to fail. Error/Blocked state, exact bridge
repair and RAII observer cleanup assertions remain. Production semantics and
timers are unchanged. The first artifact, all original logs and failed receipt
are retained; it is not the final accepted candidate. The corrected batch uses
the same cached workspace feature shape and reruns affected qualification.

## Corrected compiled qualification

The corrected cached workspace compile froze final local candidate SHA256
`b3640415148f5923cabcd24c47acbda173901df1f698575c951d07ae55aa2e7c`.
The complete affected batch passed: schema 121, federation 249, root 539, client 130,
MCP 608 and daemon 549 tests, with one existing daemon test ignored. The actual
mixed-context CLI integration passed both command routes in JSON/text; the
engine debt-during-pass control passed; four web asset controls, formatting,
all-target Clippy with warnings denied and whitespace checks passed. Every
targeted filter executed a positive test population. The corrected real policy
postflight failure test and warmed-loop/namespace controls all passed.

Clippy reused the unmodified native archive from the same workspace build through
supported library/include bindings. No dependency, repository linker setting,
production schedule or feature shape changed. Current source hashes and all32
frontend assets match the frozen candidate manifest.

## Corrected manual qualification

Native CLI/MCP acceptance passed on its first invocation, including both command
routes in JSON/text, exact valid-only identities/order, paired Same candidates,
reordering/repetition, all-ambiguous3/all-miss2, tag/UID/path and tiny-budget
controls. Actual authenticated two-tier composition retained both sources'
candidate handles and matched the eight-row valid-only merged population. Both
owned daemons exited0; protected identities and source/binary hashes stayed
unchanged. This does not claim an older-daemon runtime replay; compatible
missing-field behavior has compiled controls.

The bridge probe's first corrected-artifact attempt stopped on a real transient
GraphML export refusal during dirty live publication, after its real warm-pass
prerequisite passed. Root/judge approved handling only that exact exit1/message
with matching dirty/live owned-writer evidence as an incomplete observation.
The whole snapshot is retried within its original25s deadline; no partial export
is parsed or counted as success. Other errors remain fatal, and Current/exact
edges/marks/clean-debt success predicates remain unchanged. Original script,
refusal/status receipt and clean owned shutdown are retained. This is a harness
correction, not a product change or bridge acceptance claim.

The single approved corrected bridge run passed without any incomplete snapshot
observations. Warm, policy exclusion, ordinary refresh and reinclusion retained
the exact two note/section bridge tuples, Current inventory, clean pending state
and unchanged Keep content. Debt marks advanced2→3→4→5, once per operation, on
the same daemon. Root/judge checked its GraphML and status evidence. Owned
daemon exit0/protected unchanged; the original failed observer receipt remains.

Both author browser phases passed on their first invocation: two focused
overflow/error cases in 9.0s and nine ordinary-corpus controls in 2.1min. The latter
cover two Files keyboard cases, two folder cases, obsolete-workspace response
ownership, actual Compare JSON/Copy/Save and Impact/Related refresh/error
ownership. No skips, retries or deadline changes. All five geometry observations
placed the last row fully inside the clipped viewport with a valid pointer hit
and unscrolled ancestors. Actual HTTP index/JS/CSS hashes and all32 assets matched
the frozen candidate before/after each phase; UI/daemon cleanup0 and protected
identities unchanged.

## Independent bug hunt and local signoff

A separate judge's native first invocation checked two ambiguities plus a valid
input, genuine miss, duplicate/reordered input and both CLI routes in JSON/text
against raw MCP and valid-only ranking. A real55-candidate long-path case checked
atomic identities, paired candidate arrays, explicit omission and the disclosure
byte bound; its final response was 2074 bytes with the whole diagnostic explicitly
omitted. This is successful bounded disclosure, not proof that every diagnostic
fits every output budget.

The judge then ran an independent actual-UI journey at 1280/1200×480 with a wrapped
85px header, prefix sibling, maximum ordinary wheel scroll, reset/focus and
reselection. The tree ended at456 and its last row at432..452, pointer hit true,
all clipping ancestors unscrolled. Actual HTTP assets and full manifests stayed
unchanged. Root and judge viewed the selected/reset captures. These observations
qualify file-tree behavior; they do not assert full graph-worker settlement.

All owned judge MCP/UI/daemon processes exited0; protected identities stayed
unchanged. Independent source review, final evidence judgment and root local
signoff approve these three scoped corrections on the frozen candidate. No new
actionable regression was found in the exercised paths. Original failed setup,
fixture and observation receipts and the earlier cohort's named residuals remain
preserved.

Required CI must pass on the new exact commit before delivery is reported
complete. Green15b27f01 is historical evidence, not qualification of these
corrections. No merge, release, whole-card Done or real brain watcher change is
included.

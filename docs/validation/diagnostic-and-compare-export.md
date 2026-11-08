# Database query diagnostics and historical Compare export

This follow-on extends PR #543 with nw-944 and nw-945. The original
[41-item validation ledger](retrieval-small-backlog.md) retains its historical
artifact qualifications and residual scope. The combined PR contains 43 items;
this document qualifies only the two additional fixes and their regression round.

## Changes and causal controls

nw-944 carries typed local-query provenance from the local dispatch boundary to
the CLI diagnostic. A known selected database supplies the recovery identity;
an attributed inner store error remains authoritative. Unknown or remote storage
failures receive an identity-unavailable diagnostic without guessing a default,
previously opened file or server-private path. The marker preserves the original
error display and source chain. Query routing, storage formats and recovery
operations are unchanged.

The old standard artifact reproduced the lazy-query DiskArray corruption on an
owned, generation-qualified database but named `./nestweaver.lbug` instead of the
selected absolute file. A healthy counterpart returned its real caller. Fixture
qualification did not alter database bytes, and the corrupt overflow pointer
survived daemon startup. This is an equivalent causal fixture, not a claim about
the unavailable original user database.

nw-945 gives JSON view, Copy and Save the same bounded historical serializer.
Each snapshot retains its own whitelisted edges and eight publication metadata
fields. Endpoint eligibility uses retained canonical seed/connected UIDs before
the 1000-edge cap. Export filtering/cap counts remain separate from historical
server omission and truncation facts. Existing node caps and field whitelists
remain in force.

The old served UI reproduced the omission with real A/B API replies containing
two and three edges: JSON lacked both snapshots' edges and publication metadata.
The strengthened test failed on the missing edge field, after successful API
responses. Seven serializer cases passed, covering endpoint-before-cap ordering,
999/1000/1001 boundaries, count arithmetic, legacy/null inputs, privacy,
immutability and original server truncation facts.

## Build qualification

The final standard Debug artifact is SHA256
`0cc53c872285b70b46e2efbcbce8bed27a5ef389f2bf1c5e844a8b8a9c222304`.
It uses source-bundled native 0.22.1, CMake Debug, parsed-cache version16 and
resolver generation22. The workspace feature shape stayed fixed; no package-only
Cargo tests, CI-direct fixture context, dependency or linker changes were used.

Final cached `cargo test --workspace --no-run --locked` passed. The compiled
root suite passed 538 tests and the client suite passed 130 tests, with no
failures or ignored tests. The frontend asset guard, formatting, workspace
all-target Clippy with warnings denied and diff whitespace check passed.
Clippy reused the unmodified native archive from that same build through its
supported library/include environment bindings; it did not trigger another
native rebuild or change repository build settings.
Frontend TypeScript, targeted ESLint and the production Vite build passed.
The unchanged offline release-publication gate suite passed all 34 tests again.

An intermediate client run hit the host temporary-filesystem quota while an
unchanged test wrote its 2GiB fixture. Retained owned binary artifacts were moved
to disk-backed ignored storage with their hashes verified and original paths
preserved through symlinks. The complete final client suite then passed. The
intermediate binary was superseded after root restored the existing logical
routing anchor; it is not the final runtime qualification.

Debug RustEmbed reads frontend assets from disk. UI acceptance therefore binds
the binary above to the actual served index SHA256
`f4e6ab4ea09558cd8322c626a5989a71b7c0696b574610f115058246768baa32`
and `index-tRvg4vEe.js` SHA256
`d23f8e5d7e01c3843727036414a1d975c89e9162366f56f90ac1ac9c2f3c9411`,
with unchanged 32-file manifests before and after each browser campaign.

## Manual acceptance and final disposition

Author and independent final-artifact CLI matrices each passed eight fresh cases:
explicit/config/environment/default selection, each healthy and genuinely corrupt.
Healthy queries returned the actual caller; genuine missing symbols retained exit2.
Corrupt queries retained exit1, the DiskArray cause and the selected database in
both JSON and stderr. The causal overflow pointer survived startup and queries.
The independent precedence controls also verified that lower-priority poison DBs
were never created. Actual MCP invalid-regex envelopes matched the old standard
artifact exactly, with healthy regex counterweights. All owned fixtures and MCP
processes closed successfully; protected daemon identities remained unchanged.
Unknown/remote backing paths, stale in-process global state and typed-inner-path
precedence are qualified by the focused Rust tests, not invented external CLI
states.

A fresh four-note CLI/MCP smoke also passed the original retrieval fixes on this
artifact. Mixed ambiguous/valid seeds retained the exact valid two-row population,
reported two ambiguous candidates and kept the outer MCP result successful;
explicit tags returned four rows, misses were disclosed and all-ambiguous status
remained intact. A real watcher excluded the backup with typed not-found,
continued serving surviving context, kept an excluded save absent after a
surviving edit was indexed, then re-included its latest indexed content after an
atomic policy change. No manual refresh ran. Watcher, MCP and daemon cleanup all
returned0. The first smoke's incorrect search-response field oracle was retained
and corrected to the documented `results` field without changing its deadline.

The independent exploratory UI bug hunt passed. Local source/evidence review and
root signoff are complete; new-head CI remains required. Old-head CI success is
not acceptance for these changes.
No merge, release, production database mutation or real brain watcher restart is
included.

The first 67-case browser invocation finished with 63 passes and four failures.
The strengthened Compare case failed at the original initial detail-panel
assertion before any comparison or new export assertion. Three unchanged
workspace cases exhausted their total 30s budgets; trace timelines show the final
assertions had only milliseconds or about one second remaining. The new serializer
had not executed in these Graph-only phases. Actual relevant HTTP responses were
successful. This limits attribution; it does not prove a scheduler, CPU or timer
cause, or establish the absence of a performance regression. All original traces
remain retained. A bounded four-case old/new counterweight and an independent
exploratory journey supplied the additional acceptance below. The served asset pair
and full dist manifest were unchanged, owned UI/daemon cleanup returned0 and
protected daemon identities stayed unchanged.

The planned counterweight ran exactly once per artifact, with the same tests,
workers1/retries0 and unchanged deadlines. The old artifact finished one pass and
three failures: the known missing-edge export assertion, plus the same overall
deadlines in Search Phrases and knowledge-card cases. The candidate passed all
four cases in 1.6min: full Compare/export23.0s, Search Phrases27.8s, knowledge
cards25.6s and obsolete-workspace Files20.6s. The Compare case checked actual API
snapshots with two/three edges and publication metadata, actual clipboard content
and downloaded JSON, readable labels, canonical UIDs and identical historical
payload after layout changes. This is positive coverage for all67 distinct
browser controls across two candidate invocations, plus the seven pure serializer
cases; it is not one clean67-case invocation or proof of a timing cause. Actual
binary and served asset hashes matched each stage before/after, full32-file
manifests stayed unchanged, candidate assets were restored and both stages had
successful owned cleanup with protected processes unchanged.

An independently authored exploratory journey passed in22.1s. It compared A/C
against its own real API (two/three edges), checked actual clipboard and downloaded
JSON, changed layout without changing historical data, cleared Compare and
verified inactive/null snapshots and empty seeds in actual exports, then compared
A/B against fresh API evidence (two/two edges). This separate fixture had
generation2 rather than the author's generation3; each export retained its own
original publication facts. All three export pairs matched. Browser/UI/daemon
exited0, protected processes stayed unchanged and actual HTTP asset hashes plus
the full32-file manifest were stable. Initial screenshots show readable Compare
labels and the JSON header; they do not themselves prove visible historical
fields or settled canvas rendering. The exact API/export assertions provide
that data evidence independently of those images.

One bounded visual-only continuation passed in19.7s. A nonzero canvas/live WebGL
readiness check within the existing5s window and two animation frames produced
visible graph nodes/edges beside readable Compare labels. Inspecting actual
scroll metrics identified the overflowing ancestor rather than the unscrollable
`pre`; scrolling that ancestor and observing two frames produced a screenshot
with both retained CALLS edges and original publication metadata visible. No CSS,
product behavior or deadline changed. Root viewed these final images. This is
paint/readiness evidence, not a claim that the layout worker had fully settled.
The same final artifact/served-assets/manifest and cleanup protections passed.

Root reviewed the final source, causal baselines, all affected local gates,
author and independent runtime receipts, raw failed controls and final visual
evidence. No introduced functional regression was found in the exercised scope.
Approve these two scoped fixes for the same integration PR, retaining the
original cohort's explicit residuals. Required CI must qualify the new exact head
before delivery is reported complete; no merge or release is authorized here.

Initial manual harness failures are retained. One independent fixture requested
a different config from its daemon's lifetime binding and correctly received a
refusal before the intended query. The author harness assumed a generic MCP
message instead of the existing detailed invalid-regex envelope, and required
whole-file byte identity after daemon shutdown. Old-artifact controls also show
post-close database byte changes, so that assertion cannot establish a regression.
Generation qualification, selected identity, real query behavior and retention
of the causal corruption pointer remain required acceptance checks.

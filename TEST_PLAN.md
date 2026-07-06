# NestWeaver UI Overhaul P1 Manual Test Plan

This plan covers P1 core product value only. It is written for a later human-like testing agent to execute after P1 implementation and automated release-gate tests are accepted. Do not execute these manual cases while creating or reviewing this plan.

P1 scope under test:

- Orient to a workspace.
- Understand a symbol or note.
- Assess impact before editing.
- Trace execution flow.
- Connect code to rationale.
- Verify freshness, federation, and trust.

For every executed case, record the browser, operating system, build or commit under test, active theme, reduced-motion setting, data source or fixture name, backend mode, workspace selected at start, and whether upstream/federation simulation is enabled.

Use plain UI behavior as the source of truth. Do not rely on coded selectors, internal component names, or implementation details during manual execution.

## Required Setup Notes

- Use a P1 build with the workspace switcher, knowledge cards, deterministic Search Phrases, tri-panel workspace, graph/table/JSON representations, and Impact lens available.
- Use a representative indexed database or fixtures that include at least:
  - an all-content workspace,
  - one repo workspace,
  - one vault workspace or note-backed workspace,
  - at least one searchable symbol with callers and callees,
  - at least one searchable note with backlinks,
  - at least one traceable entry point,
  - at least one path query with either a path or an explicit no-path state,
  - at least one impact target with local blast-radius results,
  - at least one result set with truncation or continuation metadata if available,
  - trust-state fixtures or controls for local-only, org-unavailable, stale, timeout, permission rejected, read-only, and unknown states.
- If a state cannot be simulated in the current build or fixture, mark the step as blocked with the missing fixture or capability. Do not treat unavailable fixture coverage as a pass.
- Manual execution should capture visual evidence and notes, but should not implement fixes.

## TC-P1-01 Orient to a Workspace

Purpose: Verify that a user can choose a workspace, understand what is in scope, and see freshness/trust before inspecting details.

Preconditions/setup notes: Start from a clean browser session with the web UI available and at least all-content plus one repo workspace. Include a vault workspace if the fixture provides one.

1. Open the NestWeaver web UI.
   - Expected result: The app opens to a P1 workspace-oriented surface with visible workspace selection, overview, graph or scene area, representation controls, and status chrome.
2. Open the workspace selector.
   - Expected result: All-content, repo, and vault workspaces that exist in the fixture are visible with stable labels and enough context to distinguish their scope.
3. Select the all-content workspace.
   - Expected result: The overview, status chrome, and scene summary identify all-content scope and do not imply repo-only or federated authority unless that is true.
4. Select a repo workspace.
   - Expected result: The overview and graph update to the selected repo scope, and the status chrome shows the repo label plus freshness and trust state.
5. Select a vault workspace if one exists.
   - Expected result: The overview and available note/rationale surfaces update to the selected vault scope, or the UI shows an explicit unavailable state if vault scope is not supported by the fixture.
6. Inspect the overview for important hubs, bridges, clusters, suggested questions, or equivalent orientation cues.
   - Expected result: The overview is aggregate and task-shaped; it does not dump an unbounded raw symbol graph as the default daily surface.

Overall success criteria: A user can select a meaningful workspace and tell what data is in scope, what is fresh, and what can be trusted before taking action.

Evidence to capture when executed: Full-page screenshots for the initial view and each selected workspace; notes naming each workspace id or label tested; screenshot of the status chrome for each trust state visible.

## TC-P1-02 Keyboard-Only Orient Path

Purpose: Verify keyboard-only smoke coverage for the Orient journey.

Preconditions/setup notes: Start from page load. Do not use the mouse or trackpad during the steps.

1. Move focus from the browser page into the app using keyboard controls only.
   - Expected result: A visible focus indicator appears on a logical first control or app surface.
2. Navigate to the workspace selector using keyboard controls only.
   - Expected result: The selector is reachable and has a visible focus state.
3. Open the workspace selector and choose a repo workspace using keyboard controls only.
   - Expected result: The selected workspace changes, focus remains predictable, and the overview/status update is visible.
4. Move to the overview or graph scene summary using keyboard controls only.
   - Expected result: The orientation content is reachable without hover, and the non-graph summary communicates the current workspace.
5. Move to the representation controls and switch between graph and table/list if available from the overview.
   - Expected result: The active representation changes and focus stays in a usable location.

Overall success criteria: A keyboard-only user can choose a workspace, orient to the result, and reach at least one non-graph representation.

Evidence to capture when executed: Short screen recording of the keyboard path; notes for focus starting point, selected workspace, and final focus target.

## TC-P1-03 Workspace Deep Link Restore

Purpose: Verify that deep links restore workspace scope and orientation state.

Preconditions/setup notes: Use a workspace with a visible overview and a browser where copying and reloading URLs is allowed.

1. Select a repo workspace and leave the overview in graph representation.
   - Expected result: The active workspace and graph representation are visible in the UI state.
2. Copy the current page URL or use the UI copy-link action for the current scene.
   - Expected result: A shareable link is produced without an error notification.
3. Open the copied link in a new tab or reload the current tab with that URL.
   - Expected result: The same workspace scope restores, and the user lands on the same orientation lens or overview state.
4. Switch to table/list representation and repeat the copy-link and reload path.
   - Expected result: The same workspace restores with table/list representation active.
5. Switch to JSON representation and repeat the copy-link and reload path.
   - Expected result: The same workspace restores with JSON representation active and no stale previous representation flashes as the final state.

Overall success criteria: Workspace scope and representation mode survive reloads and copied links.

Evidence to capture when executed: The copied URLs, screenshots before and after reload for each representation, and notes describing any transient or final mismatch.

## TC-P1-04 Understand a Symbol or Note with Knowledge Card

Purpose: Verify that a knowledge card answers identity, role, evidence, relationships, trust, actions, and state.

Preconditions/setup notes: Use a searchable symbol with callers or callees and a searchable note with backlinks if available.

1. Search for a known symbol by ordinary search or `explain <symbol>`.
   - Expected result: Deterministic search or phrase preview shows the intended symbol or an explicit candidate list if ambiguous.
2. Open the symbol knowledge card.
   - Expected result: The card shows identity, kind, source location or note location, role/summary, evidence excerpt, relationship chips, trust/provenance, current state, and available actions.
3. Inspect actions on the card.
   - Expected result: Explore, Impact, Trace, Path, Ask, Open, and Copy link appear where supported; unsupported actions are disabled or explained instead of silently failing.
4. Activate the symbol from the card.
   - Expected result: A tri-panel workspace opens or updates with focused graph, source or note evidence, and detail/actions visible together in the selected layout.
5. Select a known note or run `explain <note>`.
   - Expected result: The note card shows identity, role or summary, evidence excerpt, backlinks or related symbols, trust/provenance, state, and supported actions.
6. Copy a link from the card and reload it.
   - Expected result: The selected symbol or note, active workspace, active lens, and representation mode restore as closely as the product contract promises.

Overall success criteria: A user can determine what the item is, why it matters, what evidence supports it, what relationships it has, what can be trusted, and what action to take next.

Evidence to capture when executed: Screenshots of compact and expanded cards; screenshots of disabled action reasons; copied deep link; before/after reload screenshots for selected item restore.

## TC-P1-05 Keyboard-Only Understand Path and Action Parity

Purpose: Verify keyboard-only access to understanding a symbol or note and parity between card actions, context menu actions, and keyboard action paths.

Preconditions/setup notes: Use only the keyboard. Use a target symbol with at least Explore, Impact, Trace, Open, and Copy link available where the fixture supports them.

1. Focus search or the command surface using keyboard controls only.
   - Expected result: Search is reachable and the focused input or command surface is clear.
2. Search for a known symbol and choose the result using keyboard controls only.
   - Expected result: The knowledge card opens or the selected result receives focus without requiring mouse hover.
3. Move through the knowledge card fields and actions using keyboard controls only.
   - Expected result: Identity, evidence, relationship chips, trust badge, state, and each action are reachable or announced in a useful order.
4. Open the context menu or equivalent keyboard action menu for the same selected item.
   - Expected result: The same supported actions are available as in the card; unavailable actions have the same reason.
5. Activate Copy link from both the card path and the keyboard/context-menu path if both are available.
   - Expected result: Both paths produce a link or the same explicit reason when unavailable.
6. Close the card or menu.
   - Expected result: Focus returns to the selected item, graph scene, or a logical nearby control.

Overall success criteria: A keyboard-only user can understand an item and invoke the same supported actions available to pointer users.

Evidence to capture when executed: Screen recording of the full keyboard path; notes comparing card actions and context-menu actions; focus-return notes.

## TC-P1-06 Search Phrase Preview, Ambiguity, and Coverage

Purpose: Verify deterministic Search Phrases, previews before expensive actions, ambiguity handling, supported phrases, limited phrases, and deliberate exclusions.

Preconditions/setup notes: Use targets that are known to resolve. Include at least one ambiguous symbol or note name. Use phrase coverage documentation from the build under test to identify phrases marked supported, limited, or deliberately excluded.

1. Enter `explain <symbol>` for an unambiguous symbol.
   - Expected result: The deterministic parser recognizes the phrase before any Ask or LLM fallback, shows a preview or result for the symbol, and offers an explain/knowledge-card path.
2. Enter `impact of <symbol>`.
   - Expected result: A preview appears before running the Impact lens, especially if the action is expensive.
3. Enter `trace flow from <symbol>`.
   - Expected result: A preview appears and clearly states that execution will open a Trace lens.
4. Enter `callers of <symbol>` and `callees of <symbol>`.
   - Expected result: Each phrase resolves the symbol and previews or opens the corresponding relationship result without silently switching relationship direction.
5. Enter `path from <A> to <B>`.
   - Expected result: Both endpoints are resolved or candidate lists are shown for each ambiguous endpoint before executing the path query.
6. Enter `tests affected by <symbol|file>`.
   - Expected result: The UI either previews affected-test hints or shows explicit limited-confidence/local-only metadata instead of pretending certainty.
7. Enter `dead code in <repo|project>`, `bridges in <repo|project>`, and `hubs in <repo|project>`.
   - Expected result: Supported phrases preview and execute; limited phrases disclose heuristic/local-only limits; unsupported phrases show explicit unsupported state.
8. Enter `notes about <topic>` and `backlinks for <note>`.
   - Expected result: Note-focused phrases resolve into note/backlink results with preview or candidate handling.
9. Enter `stale repos` and `contract drift`.
   - Expected result: Staleness is shown from freshness metadata; contract drift either returns a result or an explicit P1 unsupported/limited state with JSON parity.
10. Enter a deliberately excluded phrase documented by the build, or a non-P1 mutating phrase such as `delete repo <name>` if no deliberate exclusion is documented.
   - Expected result: The UI identifies the phrase as unsupported or excluded and does not run an unsafe or unrelated action.
11. Enter an ambiguous phrase target such as a duplicated symbol name.
   - Expected result: Candidate choices appear, enough metadata is shown to disambiguate, and no first candidate is silently selected.

Overall success criteria: The initial phrase set is either supported or explicitly marked limited/unsupported, expensive actions preview before execution, ambiguity is recoverable, and deterministic parsing precedes Ask or LLM fallback.

Evidence to capture when executed: Screenshot or recording for each phrase family; notes recording support level; screenshot of at least one ambiguous candidate list; screenshot of at least one limited or deliberately excluded phrase.

## TC-P1-07 Impact Lens End to End

Purpose: Verify the Assess Impact journey through Search Phrase, knowledge card action, representations, affected tests, source evidence, and deep links.

Preconditions/setup notes: Use a symbol with known impact results. If possible, use a fixture that can expose both local and org-tier impact data.

1. Run `impact of <symbol>` from the search or command surface.
   - Expected result: The phrase resolves the symbol, previews the expensive action, and opens the Impact lens after confirmation or selection.
2. Inspect the Impact graph.
   - Expected result: The graph is a readable layered impact view or equivalent task-shaped scene, not a random raw neighborhood, and the selected symbol remains clear.
3. Inspect the Impact summary and trust state.
   - Expected result: The lens shows whether results are two-tier, local-only, org-unavailable, stale, partial, or unknown.
4. Switch to table/list representation.
   - Expected result: Impact nodes, relationships, tiers, confidence, and state are inspectable without relying on graph position.
5. Switch to JSON representation.
   - Expected result: JSON includes nodes, relationship data or equivalent edges, affected-test hints where available, source evidence where available, and `_meta` or equivalent trust/provenance metadata.
6. Inspect affected tests.
   - Expected result: Linked tests, heuristic hints, or an explicit unavailable/limited-confidence state are shown.
7. Inspect source evidence.
   - Expected result: Source snippets, spans, files, or an explicit unavailable state are shown; the UI does not imply evidence exists when it does not.
8. Open Impact from the same symbol's knowledge card action.
   - Expected result: The same symbol and workspace are used, and results are consistent with the Search Phrase path.
9. Copy the Impact deep link and reload it.
   - Expected result: Selected symbol, workspace scope, active Impact lens, filters if visible, and representation mode restore.

Overall success criteria: Impact is usable end to end and never renders missing trust, affected-test, or evidence data as an empty success.

Evidence to capture when executed: Screenshots of graph/table/JSON Impact views; trust summary screenshot; affected-test and source-evidence screenshots; copied deep link and after-reload screenshot.

## TC-P1-08 Impact Trust State Matrix

Purpose: Verify that Impact trust and degradation states are distinct, visible, and accessible.

Preconditions/setup notes: Use trust-state fixtures, mock backend controls, or environment setup approved for manual testing. Do not implement new fixtures during manual execution.

1. Run Impact with two-tier local plus org data if available.
   - Expected result: Local and org tiers are visibly distinct and semantically described; the UI does not collapse them into an unlabeled combined result.
2. Run Impact with local-only data.
   - Expected result: The UI clearly says results are local-only and does not imply org-wide coverage.
3. Run Impact with upstream/org unavailable.
   - Expected result: The UI distinguishes org-unavailable from local-only success and from permission rejection.
4. Run Impact with stale data.
   - Expected result: Stale state is visible in the lens and status chrome and does not look current.
5. Run Impact with a timeout.
   - Expected result: Timeout is distinct from empty result, no match, and generic error; retry or continuation guidance appears where supported.
6. Run Impact with permission rejected.
   - Expected result: Permission rejection is distinct from outage and empty result.
7. Run Impact in read-only mode.
   - Expected result: Read-only capability is visible; mutating or admin actions are unavailable or gated, while read-only inspection remains usable.
8. Run Impact with truncated results.
   - Expected result: Truncation is disclosed with counts or limits where available and does not look complete.
9. Run Impact with continuation available.
   - Expected result: Continuation boundary or next action is visible and keyboard reachable if enabled.
10. Inspect affected-test and source-evidence availability in each state that can be simulated.
   - Expected result: Available evidence is linked; unavailable evidence is explicitly unavailable, limited, or unknown.

Overall success criteria: Two-tier, local-only, org-unavailable, stale, timeout, permission rejected, read-only, truncated, continuation, affected-test, and source-evidence states are each distinguishable.

Evidence to capture when executed: State-by-state screenshots or recordings; notes describing fixture used for each state; accessibility notes for state announcements if available.

## TC-P1-09 Keyboard-Only Impact Path

Purpose: Verify keyboard-only smoke coverage for the Impact journey.

Preconditions/setup notes: Use only the keyboard. Use a target symbol with Impact available.

1. Focus search or the command surface using keyboard controls only.
   - Expected result: The input surface is reachable and visibly focused.
2. Enter `impact of <symbol>` and navigate the preview using keyboard controls only.
   - Expected result: The preview can be inspected, candidate choices can be changed if present, and execution can be started without mouse input.
3. In the Impact lens, move focus through the trust summary, graph or scene summary, table/list representation, JSON representation, affected tests, and source evidence.
   - Expected result: Each area is reachable or has a keyboard-reachable equivalent.
4. Change representation mode using keyboard controls only.
   - Expected result: The active representation changes and the user's focus remains in a logical location.
5. Copy the Impact link using keyboard controls only.
   - Expected result: Copy link succeeds or gives an accessible explanation if unavailable.

Overall success criteria: A keyboard-only user can run Impact, inspect trust and evidence, change representations, and copy a link.

Evidence to capture when executed: Screen recording of the keyboard-only path; notes on any focus traps, skipped panels, or inaccessible controls.

## TC-P1-10 Trace Execution Flow and Path Queries

Purpose: Verify the Trace journey and path-between workflows with graph/table/JSON parity and continuation disclosure.

Preconditions/setup notes: Use a traceable symbol and two endpoints for a path query. Include a capped or truncated trace if the fixture provides one.

1. Run `trace flow from <symbol>`.
   - Expected result: The phrase previews and opens a typed Trace lens.
2. Inspect the Trace graph or stepper/swimlane.
   - Expected result: The flow order is understandable, selected steps are clear, and the current step is not represented only by graph position.
3. Select a trace step.
   - Expected result: The corresponding source span, note evidence, or explicit no-evidence state appears.
4. Switch Trace to table/list representation.
   - Expected result: Steps, relationships, order, evidence availability, and trust metadata are inspectable in an accessible non-graph view.
5. Switch Trace to JSON representation.
   - Expected result: JSON includes trace result data and `_meta` or equivalent provenance/trust metadata.
6. If trace is truncated or capped, inspect the continuation state.
   - Expected result: The UI discloses truncation or continuation and does not imply the trace is complete.
7. Run `path from <A> to <B>`.
   - Expected result: Endpoint candidates are resolved before execution; the result shows a path, no-path, or unsupported state explicitly.
8. Switch Path results through graph, table/list, and JSON.
   - Expected result: The same path or no-path semantics are represented consistently across all available representations.
9. Use back and forward scene history after selecting a trace step or path result.
   - Expected result: The app returns to the same trace position, selected node, and representation mode.

Overall success criteria: Users can trace behavior and inspect paths without manual file jumping, and parity views preserve order, evidence, and trust.

Evidence to capture when executed: Screenshots of Trace graph/stepper, table/list, and JSON; screenshot of selected source evidence; path result screenshots; notes on truncation or continuation.

## TC-P1-11 Connect Code to Rationale

Purpose: Verify that code, notes, ADRs, tags, sections, backlinks, and evidence connect in both directions.

Preconditions/setup notes: Use a fixture with at least one note linked to code or symbols and at least one note with backlinks.

1. Run `notes about <topic>`.
   - Expected result: Note results appear with scope, relevance, and trust/provenance metadata.
2. Open a note knowledge card.
   - Expected result: The card shows note identity, role or summary, evidence excerpt, backlinks, linked symbols or sections, trust, state, and supported actions.
3. Activate the note into the tri-panel workspace.
   - Expected result: Note evidence, related code or symbols, and detail/actions are visible together.
4. Run `backlinks for <note>`.
   - Expected result: Backlinks appear as a result set with graph/table/JSON parity or an explicit unsupported/no-result state.
5. Navigate from note evidence to a linked code symbol.
   - Expected result: The code symbol opens with source evidence and keeps a path back to the rationale context through breadcrumbs/history/backlinks.
6. Navigate from the code symbol back to the note or rationale context.
   - Expected result: The return path works through visible scene history, breadcrumbs, backlinks, or relationship chips.
7. Use Ask or Explain if available for the note or linked symbol.
   - Expected result: The surface highlights graph, code, or note evidence and does not produce a text-only answer without provenance.
8. Inspect JSON for the rationale result.
   - Expected result: JSON includes provenance/trust metadata for agent or reviewer parity.

Overall success criteria: A user can move from code to rationale and back while preserving evidence, provenance, and navigable context.

Evidence to capture when executed: Screenshots of note card, backlinks, linked code evidence, breadcrumbs/history, Ask/Explain evidence, and JSON metadata.

## TC-P1-12 Representation Parity for Core Result Sets

Purpose: Verify graph/table/JSON parity across all P1 core result-set types.

Preconditions/setup notes: Use result targets that are known to return data where possible. For unavailable capabilities, use fixtures or phrases that produce explicit unsupported, limited, or no-result states.

1. Open a search result set.
   - Expected result: Graph, table/list, and JSON representations are available or explicitly unavailable with reason.
2. Open backlinks for a note.
   - Expected result: Backlink semantics are consistent across graph, table/list, and JSON.
3. Open callers for a symbol.
   - Expected result: Caller direction is preserved across graph, table/list, and JSON.
4. Open callees for a symbol.
   - Expected result: Callee direction is preserved across graph, table/list, and JSON.
5. Open a trace result.
   - Expected result: Trace order and step evidence are preserved across graph, table/list, and JSON.
6. Open a path result.
   - Expected result: Path order, no-path state, or unsupported state is preserved across graph, table/list, and JSON.
7. Open an impact result.
   - Expected result: Impact tiers, affected tests, source evidence, and trust metadata are preserved across graph, table/list, and JSON.
8. Open hubs for a repo or project.
   - Expected result: Hub ranking, score or reason, and trust metadata are inspectable in table/list and JSON, not only by node size or position.
9. Open bridges for a repo or project.
   - Expected result: Bridge reason, connected areas, and trust metadata are inspectable in table/list and JSON, not only by color or graph position.
10. Open dead code for a repo or project.
   - Expected result: Dead-code candidates, heuristic/limited confidence, or unsupported state are visible in each available representation.
11. Open contract drift.
   - Expected result: Contract drift results appear with parity if supported; otherwise unsupported or limited state appears consistently in graph/table/JSON or an explicit non-graph state.
12. For each table/list view, navigate rows using keyboard controls.
   - Expected result: Tables use accessible semantics, visible focus, and row activation without mouse hover.
13. For each JSON view, inspect metadata.
   - Expected result: JSON includes `_meta` or equivalent provenance/trust metadata when available; missing metadata is treated as a failure or blocked fixture issue, not silently ignored.

Overall success criteria: Every P1 result set has useful non-graph equivalents, and unsupported-state results preserve semantics across available views.

Evidence to capture when executed: A parity matrix listing each result type and whether graph/table/JSON passed; screenshots of representative table/list and JSON views; notes for unsupported or limited capabilities.

## TC-P1-13 Selected Node, Lens, and Representation Deep Link Restore

Purpose: Verify deep-link restore for selected node, active lens, and representation mode beyond the workspace-only path.

Preconditions/setup notes: Use a selected symbol with Explore, Trace, and Impact available where possible.

1. Open a symbol in Explore or focused detail mode and select a specific node.
   - Expected result: The selected node is clear in graph and non-graph surfaces.
2. Switch to table/list representation, copy the link, and reload it.
   - Expected result: The same selected node and table/list representation restore.
3. Switch to JSON representation, copy the link, and reload it.
   - Expected result: The same selected node and JSON representation restore.
4. Open Trace for the symbol, select a trace step, copy the link, and reload it.
   - Expected result: Active Trace lens, selected trace step or selected node, workspace, and representation restore.
5. Open Impact for the symbol, select table/list or JSON, copy the link, and reload it.
   - Expected result: Active Impact lens, selected symbol, workspace scope, filters if visible, and representation restore.
6. Change workspace after restoring a deep link.
   - Expected result: The app either updates the scene honestly for the new workspace or shows an explicit unsupported/no-result state if the selected node is out of scope.

Overall success criteria: Deep links restore active workspace scope, selected node, active lens, and representation mode across reloads and copied links.

Evidence to capture when executed: Copied URLs; screenshots before and after reload for Explore, Trace, and Impact; notes on any restored filters or out-of-scope states.

## TC-P1-14 Freshness, Federation, and Trust Visibility

Purpose: Verify the Freshness/Federation journey and general trust semantics across UI surfaces.

Preconditions/setup notes: Use trust-state fixtures or backend modes that can expose local-only, federated, read-only, upstream-unreachable, indexing, stale, partial, and unknown states.

1. Open the UI in a current local-only state.
   - Expected result: Status chrome and relevant result metadata say local-only; no surface implies federated coverage.
2. Open the UI in a federated or two-tier state if available.
   - Expected result: Local and upstream/org contribution are visible in status, result summaries, or metadata.
3. Simulate upstream unreachable.
   - Expected result: Upstream outage is visible and distinct from permission rejected, empty, and local-only success.
4. Simulate permission rejected.
   - Expected result: Permission rejection is visible and distinct from outage or no results.
5. Simulate stale data.
   - Expected result: Stale repos or stale result state are visible and do not look current.
6. Simulate indexing in progress.
   - Expected result: Indexing state is visible, progress or pending state is shown where available, and the active scene remains usable.
7. Simulate read-only mode.
   - Expected result: Read-only state is visible; unsafe or mutating actions are gated while inspection remains possible.
8. Simulate partial or unknown freshness.
   - Expected result: Partial and unknown states are named explicitly and do not look complete or current.
9. Trigger a status update while focus is inside a result or card.
   - Expected result: The status update is visible and announced accessibly without stealing focus or randomizing the scene.

Overall success criteria: Users can tell what data they can trust before acting, and trust changes are visible, semantic, and accessible.

Evidence to capture when executed: Screenshots for each trust state; notes comparing labels and announcements; screen recording for live status update if available.

## TC-P1-15 Empty, No-Result, Ambiguous, Unsupported, Unknown, and Error States

Purpose: Verify that the UI distinguishes empty from unknown from error and handles ambiguous or unsupported results honestly.

Preconditions/setup notes: Use fixtures or queries that intentionally produce each state.

1. Search for a query that has no matches.
   - Expected result: The UI shows a no-match state, not a loading spinner, unknown state, or backend error.
2. Open a workspace or query result that is genuinely empty.
   - Expected result: Empty state explains that no content exists in scope and does not imply failure.
3. Run a phrase with an ambiguous target.
   - Expected result: Candidate choices are shown; the UI does not silently select the first candidate.
4. Run a phrase unsupported in P1.
   - Expected result: Unsupported state is explicit, stable, and available in table/list or JSON where relevant.
5. Simulate unknown freshness or unknown result completeness.
   - Expected result: Unknown is named and does not look like empty or complete success.
6. Simulate a backend error.
   - Expected result: Error is visible, announced where possible, and distinct from empty, no match, unsupported, timeout, and permission rejection.
7. Simulate a cancelled operation if cancellation is supported.
   - Expected result: Cancelled state is visible or announced and no stale result overwrites the current scene afterward.
8. Retry after an error or cancelled operation if retry is offered.
   - Expected result: Retry either succeeds or reports the next state clearly; the previous error does not remain as the active result after a successful retry.

Overall success criteria: The UI never lets empty mean unknown, never lets rejection mask outage, and never lets unsupported or ambiguous states look like successful empty results.

Evidence to capture when executed: Screenshots for each state; notes on exact wording used; recording of cancellation/retry if available.

## TC-P1-16 Accessibility, Reduced Motion, and Non-Visual Equivalents

Purpose: Verify that no core meaning depends only on color, hover, animation, or graph position and that reduced motion preserves meaning.

Preconditions/setup notes: Use a browser and operating system environment where reduced-motion preference can be enabled before load. Use at least Orient, Understand, Impact, and one relationship result.

1. Enable reduced motion before opening the UI.
   - Expected result: The UI starts in reduced-motion behavior without requiring an in-app toggle.
2. Open Orient, Understand, and Impact scenes with reduced motion enabled.
   - Expected result: Nonessential travel, ripple, breathing, and motion-heavy effects are removed or minimized; static labels, text, shape, size, rings, tables, summaries, or badges preserve meaning.
3. Inspect graph kind, selection, trust, risk, confidence, hub, bridge, and active-path states.
   - Expected result: No meaning is communicated by color alone; there is text, shape, icon, table value, badge, summary, or another non-color cue.
4. Try to complete the same core information check without hover.
   - Expected result: Required details and actions are available by click, keyboard focus, card, menu, table/list, or summary.
5. Inspect a graph result and its table/list equivalent.
   - Expected result: The non-graph equivalent is sufficient to complete the task, not merely present.
6. Use keyboard controls to reach search, node selection or row selection, preview actions, expansion chips, context menu, filters, and representation toggles where visible.
   - Expected result: Each control is reachable and has visible focus or an accessible equivalent.
7. Open modal and popover surfaces used by P1 workflows.
   - Expected result: Focus trap and focus return work for modal surfaces, and popovers can be operated without hover.
8. Observe async updates or result changes with assistive technology or accessibility tooling if available.
   - Expected result: Meaningful state changes are announced through live regions or equivalent accessible feedback.

Overall success criteria: Core P1 journeys remain viable for keyboard-only and reduced-motion users, and graph information has useful text/table/JSON equivalents.

Evidence to capture when executed: Reduced-motion recording; screenshots showing non-color cues; keyboard reachability notes; accessibility announcement notes where available.

## TC-P1-17 Tri-Panel Workspace, Evidence, and Scene History

Purpose: Verify that activation opens a usable graph/source-or-note/detail workspace with reversible navigation.

Preconditions/setup notes: Use a symbol with source spans or snippets and a note with evidence if available.

1. Activate a symbol from search or a knowledge card.
   - Expected result: A tri-panel or equivalent workspace shows focused graph, source/note evidence, and detail/actions together in non-zen layouts.
2. Select a graph node with source span evidence.
   - Expected result: Source or note evidence cross-highlights where spans/snippets allow, or an explicit no-span state appears.
3. Expand a typed relationship such as callers, callees, tests, imports, backlinks, or routes.
   - Expected result: The scene updates incrementally and preserves user orientation as much as the current data allows.
4. Use breadcrumbs.
   - Expected result: Breadcrumbs show the current scene path and can return to prior scenes.
5. Use undo and redo controls or shortcuts where available.
   - Expected result: Scene history moves backward and forward without losing selected node, lens, or representation state.
6. Use minimap and fit-to-selection controls where available.
   - Expected result: The controls are visible, keyboard reachable, and help recover orientation without hiding detail/evidence panels.
7. Trigger or observe a scene update that does not change identity/topology.
   - Expected result: Existing node positions, selected node, and camera orientation remain stable.

Overall success criteria: Activating an item creates a useful workspace for understanding evidence and moving through relationships without losing orientation.

Evidence to capture when executed: Screenshots of tri-panel layout, cross-highlight evidence, breadcrumbs, history controls, minimap/fit controls, and before/after scene update.

## TC-P1-18 P0 Regression Companion Checks

Purpose: Verify that P1 did not regress P0 foundation interactions that P1 depends on.

Preconditions/setup notes: These are companion checks for manual release confidence. They do not replace the P0 automated regression suite.

1. Open the command or query dialog with Cmd+K on macOS or Ctrl+K on non-macOS.
   - Expected result: The dialog opens, has a clear title, receives focus, and closes with focus returning to the opener or a logical app surface.
2. Open the shortcut overlay with `?`.
   - Expected result: The overlay opens, traps focus while open, lists useful shortcuts, and returns focus on close.
3. Use visible mode or lens tabs.
   - Expected result: Tabs are visible, keyboard reachable, and active state is clear.
4. Trigger a user-facing notification or feedback state such as loading, progress, success, cancellation, or error.
   - Expected result: Feedback is visible and accessible; no user-facing failure disappears silently.
5. Load the graph canvas in an overview or focused scene.
   - Expected result: The canvas is nonblank, framed, and not hidden behind controls.
6. Switch graph/list/matrix or graph/table/JSON controls where P0/P1 overlap.
   - Expected result: Existing graph explorer representations remain usable and do not block the P1 representation controls.
7. Trigger two quick successive graph or detail requests if practical.
   - Expected result: A stale earlier response does not overwrite the newer selected scene.

Overall success criteria: P0 command dialogs, focus return, mode tabs, notifications, and graph nonblank behavior remain intact while P1 features are active.

Evidence to capture when executed: Screenshots or recordings for command dialog, shortcut overlay, mode tabs, notification, graph canvas, and stale-response behavior.

## Automated-Test Companion Expectations

These companion checks are not manual steps for this plan writer. They identify the expected automated coverage that should run alongside later manual execution.

1. P1 Playwright release gates cover workspace selection and URL restore.
   - Expected result: Automated evidence includes at least one passing workspace deep-link restore path.
2. P1 Playwright release gates cover Search Phrase preview, ambiguity, and at least one unsupported or limited phrase.
   - Expected result: Automated evidence proves deterministic parsing and explicit unsupported/limited behavior.
3. P1 Playwright release gates cover knowledge card identity, evidence, trust, relationships, and actions.
   - Expected result: Automated evidence proves the card exposes the required fields and at least one action path.
4. P1 Playwright release gates cover graph/table/JSON switching on at least one search or detail result and one Impact result.
   - Expected result: Automated evidence proves non-graph parity paths are wired.
5. P1 Playwright release gates cover Impact lens trust metadata and deep-link restore.
   - Expected result: Automated evidence proves the active lens and trust state restore from URL.
6. P1 Playwright release gates cover keyboard-only Orient, Understand, and Impact smoke paths.
   - Expected result: Automated evidence proves these three journeys can be driven without pointer input.
7. P0 regression suites remain green for command dialogs, focus return, mode tabs, notifications, and graph nonblank canvas.
   - Expected result: Automated evidence confirms P1 did not regress the P0 foundation.

Overall success criteria: Manual evidence and automated release gates agree on workspace restore, Search Phrases, knowledge cards, representation parity, Impact trust state, keyboard coverage, and P0 regressions.

Evidence to capture when executed: Automated test command output, screenshots/videos/traces produced by the automated run, and a note mapping any failures back to manual case ids.

## P1 Acceptance Gate Coverage Map

| P1 acceptance gate | Manual test cases | Automated companion |
|---|---|---|
| All six journeys have a working prototype path: orient, understand, impact, trace, rationale, freshness/federation | TC-P1-01, TC-P1-04, TC-P1-07, TC-P1-10, TC-P1-11, TC-P1-14 | Automated companion checks 1, 3, 4, 5 |
| Knowledge cards answer identity, role, evidence, trust, relationships, and next action | TC-P1-04, TC-P1-05, TC-P1-11 | Automated companion check 3 |
| Search Phrases cover the initial set or document deliberate exclusions | TC-P1-06 | Automated companion check 2 |
| Impact lens works end to end with explicit trust states | TC-P1-07, TC-P1-08, TC-P1-09 | Automated companion check 5 |
| Graph, table/list, and JSON parity exists for core result sets | TC-P1-03, TC-P1-07, TC-P1-10, TC-P1-11, TC-P1-12, TC-P1-13 | Automated companion check 4 |
| Deep links restore active scope, node, lens, and representation mode | TC-P1-03, TC-P1-04, TC-P1-07, TC-P1-13 | Automated companion checks 1 and 5 |
| Keyboard-only smoke coverage includes Orient, Understand, and Impact | TC-P1-02, TC-P1-05, TC-P1-09, TC-P1-16 | Automated companion check 6 |
| Accessibility and reduced motion preserve meaning | TC-P1-16, TC-P1-18 | Automated companion checks 6 and 7 |
| Empty, no-result, ambiguous, unsupported, unknown, and error states are distinct | TC-P1-06, TC-P1-08, TC-P1-14, TC-P1-15 | Automated companion check 2 |
| P0 command dialogs, focus return, mode tabs, notifications, and graph nonblank canvas do not regress | TC-P1-18 | Automated companion check 7 |

## Manual Execution Exit Checklist

- Every TC-P1 case is passed, blocked by a named fixture/capability gap, or failed with severity and reproduction notes.
- Every blocked state names the missing fixture, data shape, or environment setup needed to execute it later.
- Every failed step includes expected result, actual result, reproduction path, evidence, and severity.
- Every deliberate P1 limitation or exclusion is visible in the UI and captured as evidence.
- Manual findings are cross-checked against the automated companion results before P1 is called complete.

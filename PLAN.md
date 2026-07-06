# NestWeaver UI Overhaul - P1 Orchestration Plan

Branch: `feature/nestweaver-ui-overhaul`

Current phase: P1 - Core product value

Spec sources read:
- `/Users/korykehl/brain/Workspaces/NestWeaver/notes/2026-07/prd/nestweaver-ui-overhaul-prd.md`
- `/Users/korykehl/dev/workspaces/nestweaver/docs/superpowers/plans/2026-07-06-nestweaver-ui-p1-core-workspace.md`

P0 status:
- P0 is complete and committed.
- Do not reopen P0 except for regressions found while validating P1.

Status legend: `pending`, `in_progress`, `review`, `complete`, `blocked`

## Phase Rules

- Only P1 is active in this plan.
- Do not plan or implement P2 until all P1 gates pass and P1 is committed.
- Implementation and testing must be done by sub-agents.
- The orchestrator may plan, delegate, review, coordinate, verify, update this plan, and commit accepted work.
- Every implementation task has exclusive file ownership. No two active agents may own the same file.
- Review agents are read-only.
- Manual testing agents may not edit source files.
- If a task fails three times for the same reason, escalate to the user.

## P1 Acceptance Gates

- All six PRD journeys have a working prototype path: orient, understand, impact, trace, rationale, and freshness/federation.
- Knowledge cards answer identity, role, evidence, trust, relationships, and next action.
- Search Phrases cover the initial phrase set or document deliberate exclusions in the UI and test evidence.
- Impact lens works end to end with explicit trust states.
- Graph, table/list, and JSON parity exists for core result sets.
- Deep links restore active scope, node, lens, and representation mode.
- Keyboard-only smoke coverage includes Orient, Understand, and Impact.

## Implementation Tasks

### Task 0 - Manual Test Plan

Status: complete

Owner type: manual-test-plan sub-agent

Files owned:
- `TEST_PLAN.md`

Objective:
- Create the P1 manual test plan before manual testing begins.
- Use plain-English user steps, not coded selectors.
- Include named test cases with numbered steps, expected result per step, and overall success criteria.
- Cover all six journeys, keyboard-only paths, deep links, Search Phrase ambiguity, graph/table/JSON parity, Impact trust states, and unsupported/exclusion states.

Acceptance criteria:
- `TEST_PLAN.md` exists.
- Every P1 acceptance gate has at least one manual test case or explicit automated-test companion.
- The agent does not execute tests and does not edit source files.

### Task 1 - Shared P1 Contracts, State, and Scoped APIs

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/src/routes/workspaces.rs`
- Create `crates/nestweaver-web/tests/p1_workspace_api_test.rs`
- Create `crates/nestweaver-web/frontend/src/api/p1Types.ts`
- Create `crates/nestweaver-web/frontend/src/api/workspaces.ts`
- Create `crates/nestweaver-web/frontend/src/stores/workspaceSlice.ts`
- Create `crates/nestweaver-web/frontend/src/stores/sceneSlice.ts`
- Modify `crates/nestweaver-web/src/routes/mod.rs`
- Modify `crates/nestweaver-web/src/lib.rs`
- Modify `crates/nestweaver-web/src/routes/overview.rs`
- Modify `crates/nestweaver-web/src/routes/context.rs`
- Modify `crates/nestweaver-web/src/routes/brain.rs`
- Modify `crates/nestweaver-web/frontend/src/stores/index.ts`
- Modify `crates/nestweaver-web/frontend/src/stores/graphSlice.ts`

Objective:
- Add a P1 workspace catalog contract: all indexed content, repo scopes, and vault scopes.
- Make active workspace state first-class in the frontend store.
- Replace dead repo/vault scope state with workspace-backed state or remove it.
- Add reusable scene metadata state for active lens, representation mode, trust, provenance, truncation, and continuation.
- Make overview, brain context, and brain search accept workspace scope where current backend data can filter honestly.

Required verification:
- `cargo test -p nestweaver-web p1_workspace`
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- `/api/v1/workspaces` returns all-content, repo, and vault workspace entries with stable ids, labels, counts where available, and local-only trust metadata.
- Overview, brain context, and brain search do not ignore an active workspace when the request is scoped to a repo or vault.
- Unsupported scope combinations return explicit empty, unsupported, or partial metadata instead of pretending all data was scoped.
- Frontend store exposes active workspace, workspace list load state, trust summary, active lens, representation mode, and scene metadata.
- No visible UI changes are required in this task.

### Task 2 - Workspace Switcher, Status Chrome, and Deep Links

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/src/components/workspace/WorkspaceSwitcher.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/WorkspaceStatusChip.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/WorkspaceScopeSummary.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/TopBar.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/StatusBar.tsx`
- Modify `crates/nestweaver-web/frontend/src/hooks/useDeepLink.ts`
- Modify `crates/nestweaver-web/frontend/src/components/overview/OverviewCommandShelf.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/overview/OverviewContextSurface.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/modes/useOverviewMode.ts`
- Modify `crates/nestweaver-web/frontend/src/components/graph/modes/useContextMode.ts`

Objective:
- Add a visible project/workspace switcher and workspace status chrome.
- Thread active workspace into overview and context loads.
- Extend deep links to restore workspace scope, selected node, active lens, and representation mode.
- Keep the overview as the project orientation surface.

Required verification:
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- Users can select active workspace scope from visible UI.
- Overview and context scenes reload for the selected workspace and disclose local-only or partial scope state.
- Status bar shows workspace label plus freshness/trust state without hiding SSE/WASM state.
- URL parameters restore workspace, selected node, active lens, and representation mode after reload.
- Existing P0 keyboard shortcuts and modal focus behavior still work.

### Task 3 - Deterministic Search Phrases

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/src/searchPhrases/types.ts`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/parser.ts`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/resolve.ts`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/execute.ts`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/PhrasePreview.tsx`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/phraseCoverage.ts`
- Create `crates/nestweaver-web/frontend/src/searchPhrases/index.ts`
- Modify `crates/nestweaver-web/frontend/src/components/SearchDropdown.tsx`
- Modify `crates/nestweaver-web/frontend/src/stores/searchSlice.ts`

Objective:
- Parse deterministic Search Phrases before any LLM fallback.
- Resolve symbols, notes, repos, projects/workspaces, and two-endpoint phrases with candidate previews.
- Execute supported phrases into typed scenes or detail focus.
- Show explicit unsupported or deliberate-exclusion results for P1-deferred capabilities.

Initial phrase set:
- `explain <symbol|note|repo|project>`
- `impact of <symbol>`
- `trace flow from <symbol>`
- `callers of <symbol>`
- `callees of <symbol>`
- `path from <A> to <B>`
- `tests affected by <symbol|file>`
- `dead code in <repo|project>`
- `bridges in <repo|project>`
- `hubs in <repo|project>`
- `notes about <topic>`
- `backlinks for <note>`
- `stale repos`
- `contract drift`

Required verification:
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- Deterministic parser runs before Ask/LLM fallback.
- Ambiguous symbols or notes show candidates and do not silently choose the first result.
- Expensive actions show a preview before execution.
- Results include provenance, freshness, truncation, continuation, or explicit unsupported metadata where available.
- The phrase coverage file documents supported, limited, and deliberately excluded behavior for every initial phrase.

### Task 4 - Knowledge Card and Action Parity

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/src/components/knowledge/KnowledgeCard.tsx`
- Create `crates/nestweaver-web/frontend/src/components/knowledge/KnowledgeActionGrid.tsx`
- Create `crates/nestweaver-web/frontend/src/components/knowledge/TrustBadge.tsx`
- Create `crates/nestweaver-web/frontend/src/components/knowledge/RelationshipChips.tsx`
- Create `crates/nestweaver-web/frontend/src/components/knowledge/JsonEvidence.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/NodePreviewCard.tsx`
- Modify `crates/nestweaver-web/frontend/src/hooks/useNodePreview.ts`
- Modify `crates/nestweaver-web/frontend/src/components/actions/useNodeActions.ts`
- Modify `crates/nestweaver-web/frontend/src/components/actions/NodeActionBar.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/ContextMenu.tsx`

Objective:
- Replace the simple node preview with a compact knowledge card and expandable detail card.
- Add action parity across card buttons, keyboard paths, and context menu.
- Add Copy link and explicit trust/provenance/state display.

Required verification:
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- Compact card shows identity, role, evidence excerpt, relationships, trust, actions, and state.
- Actions include Explore, Impact, Trace, Path, Ask, Open, and Copy link where supported.
- Disabled actions explain why they are unavailable.
- Context menu exposes the same supported actions and no longer has silent action failures.
- Preview fetches are cancellable and stale preview responses cannot overwrite newer selections.

### Task 5 - Tri-Panel Workspace and Representation Parity

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/src/components/workspace/SceneBreadcrumbs.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/WorkspaceToolbar.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/RepresentationTabs.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/JsonResultView.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/SourceEvidencePanel.tsx`
- Create `crates/nestweaver-web/frontend/src/components/workspace/LensSummaryPanel.tsx`
- Modify `crates/nestweaver-web/frontend/src/App.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/NodeListView.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/graph/GraphMatrixView.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/detail/DetailPanel.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/detail/SymbolDetail.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/detail/NoteDetail.tsx`
- Modify `crates/nestweaver-web/frontend/src/components/detail/CodePreview.tsx`
- Modify `crates/nestweaver-web/frontend/src/hooks/useNavigationHistory.ts`

Objective:
- Make node activation open a usable graph/source-or-note/detail workspace.
- Add visible breadcrumbs, undo/redo, minimap, fit-to-selection, and representation controls.
- Make graph, table/list, and JSON views available for core result sets.
- Cross-highlight graph selection with source/note evidence where current spans/snippets allow.

Required verification:
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- Activating a node shows focused graph, evidence, and detail/actions together in non-zen layouts and remains usable in focus-map layouts.
- Table/list and JSON views expose impact, trace, search, backlinks, callers/callees, dead code, hubs, bridges, and contract-drift or unsupported-state results.
- JSON includes `_meta` or equivalent trust/provenance metadata when available.
- Tables use accessible semantics and keyboard navigation.
- Scene history controls are visible and work with keyboard shortcuts.

### Task 6 - Impact Lens End to End

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/src/api/impactLens.ts`
- Create `crates/nestweaver-web/tests/p1_impact_api_test.rs`
- Modify `crates/nestweaver-web/src/routes/impact.rs`
- Modify `crates/nestweaver-web/frontend/src/components/graph/modes/useImpactMode.ts`
- Modify `crates/nestweaver-web/frontend/src/components/graph/utils/buildGraphFromImpact.ts`

Objective:
- Upgrade Impact from a raw node array into the first complete P1 lens.
- Return and display layered DAG data, affected-test hints, source evidence links, trust states, truncation, and continuation metadata.
- Distinguish two-tier, local-only, org-unavailable, stale, timeout, permission rejected, and read-only states as explicit states.

Required verification:
- `cargo test -p nestweaver-web p1_impact`
- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run build`

Acceptance criteria:
- Impact endpoint returns an envelope with `nodes`, `edges` or equivalent relationship data, affected tests, source evidence, and `_meta`.
- Frontend impact hook stores active lens metadata and graph data without stale response overwrite.
- Impact graph uses deterministic or layout-preserved positions and a layered DAG layout.
- Local-only/org-unavailable/read-only/stale/timeout/permission states are not rendered as empty success.
- Deep links restore selected symbol, workspace scope, view mode, and active impact lens.

### Task 7 - Automated P1 Release Gate Tests

Status: pending

Owner type: implementation sub-agent

Files owned:
- Create `crates/nestweaver-web/frontend/e2e/ui-p1-core-workspace.spec.ts`

Objective:
- Add Playwright release gates for P1 core workspace behavior.
- Use existing roles/test ids wherever possible.
- Do not modify source files in this task.

Required verification:
- `cd crates/nestweaver-web/frontend && npm run test:e2e -- ui-p1-core-workspace.spec.ts`
- `cd crates/nestweaver-web/frontend && npm run test:e2e -- ui-p0-foundation.spec.ts graph-explorer.spec.ts`

Acceptance criteria:
- E2E covers workspace selection and deep-link restore.
- E2E covers Search Phrase preview, ambiguity, and at least one unsupported-state phrase.
- E2E covers knowledge card identity/evidence/trust/actions.
- E2E covers graph/table/JSON switching.
- E2E covers Impact lens trust metadata and deep-link restore.
- Test failures are reported as issues; testing agents do not fix source.

### Task 8 - P1 Documentation Links

Status: pending

Owner type: documentation sub-agent

Files owned:
- `/Users/korykehl/brain/Workspaces/NestWeaver/notes/2026-07/prd/nestweaver-ui-overhaul-prd.md`
- `/Users/korykehl/brain/Workspaces/NestWeaver/backlog/nw-021-ui-overhaul-spike.md`

Objective:
- Link the P1 implementation plan from the PRD and backlog.
- Add only missing link text.
- Report whether the brain vault note edits are outside the code repo commit.

Required verification:
- Inspect both owned files before editing.

Acceptance criteria:
- PRD implementation index points to `/Users/korykehl/dev/workspaces/nestweaver/docs/superpowers/plans/2026-07-06-nestweaver-ui-p1-core-workspace.md`.
- Backlog P1 subtask includes the same implementation plan path.
- No unrelated note content is changed.

## Review Tasks

For each implementation task:
- Dispatch a read-only spec compliance reviewer after the implementer reports done.
- Compare actual code against the P1 PRD and this plan.
- If spec review fails, dispatch a fix agent with the same file ownership.
- Dispatch a read-only code quality reviewer only after spec compliance passes.
- If code review fails, dispatch a fix agent with the same file ownership.
- Mark the task complete in this plan immediately after both reviews pass.

For critical or complex tasks, dispatch a second independent reviewer:
- Task 1 - Shared P1 Contracts, State, and Scoped APIs
- Task 3 - Deterministic Search Phrases
- Task 5 - Tri-Panel Workspace and Representation Parity
- Task 6 - Impact Lens End to End
- Task 7 - Automated P1 Release Gate Tests

## Manual Test Plan Summary

The dedicated manual-test-plan sub-agent will write the full plan to `TEST_PLAN.md`. It must include at least these cases.

### MT-01 Orient to a Workspace

1. Open the web UI.
   - Expected: workspace switcher, overview, graph panel, and status chrome appear.
2. Select all-content, repo, and vault workspaces where available.
   - Expected: overview and status update for the selected scope.
3. Copy or reload a deep link.
   - Expected: the same workspace orientation is restored.

Overall success: A user can choose a project/workspace and see what data is trustworthy.

### MT-02 Understand a Symbol or Note

1. Search for a symbol or note.
   - Expected: deterministic results or phrase previews appear.
2. Open the knowledge card.
   - Expected: identity, role, evidence, relationships, trust, actions, and state are visible.
3. Activate the item.
   - Expected: focused graph, evidence, and detail/actions remain visible.

Overall success: A user can answer what the item is, why it matters, and what to do next.

### MT-03 Search Phrases

1. Run each initial Search Phrase.
   - Expected: supported phrases preview and execute; limited or excluded phrases show explicit metadata.
2. Run an ambiguous symbol phrase.
   - Expected: candidates appear and no silent first result is selected.
3. Use keyboard only.
   - Expected: phrase previews and candidates are reachable.

Overall success: deterministic commands run before LLM fallback and ambiguity is recoverable.

### MT-04 Impact Lens

1. Run `impact of <symbol>` from search and from a card action.
   - Expected: Impact lens opens with layered graph, table/list, JSON, and trust summary.
2. Inspect affected tests and source evidence.
   - Expected: links or explicit unavailable state are shown.
3. Reload an impact deep link.
   - Expected: selected symbol, workspace, lens, filters, and representation restore.

Overall success: Impact is usable end to end and never hides trust state as an empty graph.

### MT-05 Trace, Path, and Rationale

1. Run `trace flow from <symbol>`.
   - Expected: trace scene and non-graph representation appear.
2. Run `path from <A> to <B>`.
   - Expected: path candidates or no-path state are explicit.
3. Run `notes about <topic>` and `backlinks for <note>`.
   - Expected: note evidence, backlinks, JSON, and graph/table paths are available.

Overall success: code and rationale journeys work without mouse hover.

### MT-06 Representation and Accessibility

1. Toggle graph, table/list, and JSON for core result sets.
   - Expected: every representation remains useful and keyboard reachable.
2. Use breadcrumbs, undo/redo, minimap, and fit-to-selection.
   - Expected: scene navigation is visible and reversible.
3. Repeat key paths with reduced motion active.
   - Expected: no meaning depends on motion, hover, or color alone.

Overall success: graph answers have accessible non-graph equivalents.

## Final P1 Verification

Status: pending

After all tasks pass review and manual issue cycles are closed, dispatch a testing sub-agent to run the complete phase test plan. Then run or verify these commands through testing agents:

- `cd crates/nestweaver-web/frontend && npx tsc -b --noEmit`
- `cd crates/nestweaver-web/frontend && npm run lint`
- `cd crates/nestweaver-web/frontend && npm run build`
- `cd crates/nestweaver-web/frontend && npm run test:e2e -- ui-p1-core-workspace.spec.ts ui-p0-foundation.spec.ts graph-explorer.spec.ts`
- `cargo test -p nestweaver-web`

P1 can be marked complete only when:
- every task above is complete,
- every review passed,
- every manual test case passed or has a PRD-approved documented exclusion,
- every discovered issue is resolved,
- this plan is updated to complete,
- the completed P1 work is committed with conventional commits.

# Guided Overview Map Design

## Purpose

NestWeaver should feel useful the moment the web UI opens. A user should not need to know which graph mode to choose, what to search for, or what abbreviated toolbar controls mean before the interface gives them a clear starting point.

The UI should become a guided map of the indexed codebase and knowledge base: it opens to a meaningful overview, explains why visible items matter, and offers obvious next actions for common investigation jobs.

## Problem

The current UI has strong graph capabilities, but the first-open experience is too dependent on prior knowledge.

- The default graph mode is `context`, but context mode only loads when `seeds.length > 0`, so a user can open the app with no useful graph until they search or select something.
- The graph toolbar exposes powerful controls through terse labels such as `C`, `#`, `M`, `U`, `S`, `Diff`, and `Gap`. These are compact, but they make the primary affordances hard to discover.
- The empty detail panel tells users to click or search, but does not suggest what is worth clicking first.
- Mode labels such as Context, Impact, Repos, Features, and Local describe internal graph concepts more than user goals.

## Design Goal

Open to a **Guided Overview Map**:

1. A populated overview graph appears automatically.
2. A "Start Here" rail lists high-value entry points.
3. Selecting any node explains why it matters.
4. The detail panel offers contextual next actions.
5. Advanced controls are grouped by intent rather than shown as unexplained single-character buttons.

## Why Look And Feel Is In Scope

This work is not only about adding more information to the screen. The graph view is NestWeaver's primary spatial metaphor, so its visual feel directly affects whether users trust it, understand it, and want to explore it.

Obsidian's graph feels useful partly because it is fluid: nodes settle naturally, zooming and panning feel direct, labels appear at the right moments, and controls feel like part of the graph rather than a separate expert console. NestWeaver should aim for that same sense of immediate manipulability while keeping its existing color palette and brand language.

The goal is not a new visual identity. Keep the current NestWeaver colors. Improve composition, motion, density, affordances, labeling, and control clarity.

The layout itself is also in scope. NestWeaver should not be limited to the common graph-app pattern of left browser, center graph, right inspector, and bottom filters. That layout is a useful baseline, but it is not the product vision. The design should explore a layout that feels specific to NestWeaver's job: turning code, notes, and graph intelligence into an explorable map of what matters.

## Research Principles

The design follows a few durable visualization and product patterns.

- **Overview first, zoom/filter, details on demand.** Shneiderman's visual information-seeking mantra maps well to NestWeaver: show an overview, let the user narrow it, then reveal details when they choose a node. Source: `https://www.cs.umd.edu/~ben/papers/Shneiderman1996eyes.pdf`.
- **Start with user tasks, not renderer features.** Munzner's nested model frames visualization work around domain problem, data/task abstraction, visual encoding, and algorithm. The highest-risk failure is building a polished visualization for the wrong task. Source: `https://vis.csail.mit.edu/classes/6.859/readings/pdfs/Munzner-ANestedModelForVisualizationDesignAndValidation.pdf`.
- **Keep node-link graphs for local paths, but provide alternate views for dense exploration.** Ghoniem, Fekete, and Castagliola found node-link diagrams are useful for small/local path tasks, while matrix-style views scale better for larger dense graphs. Source: `https://courses.ischool.berkeley.edu/i247/f05/readings/Ghoniem-GraphReadability_InfoVis04.pdf`.
- **Use search as a scene builder.** Neo4j Bloom's product model treats search phrases as a way to create an explorable graph scene, then lets users expand from results. Source: `https://neo4j.com/docs/bloom-user-guide/current/bloom-visual-tour/bloom-overview/`.
- **Make graph controls understandable through visible groups and filters.** Obsidian's graph view is immediately useful because it opens populated and exposes filters, groups, display controls, forces, and local graph behavior in a direct way. Source: `https://obsidian.md/help/Plugins/Graph%2Bview`.

## Target Users

- **New evaluator:** Opens NestWeaver after indexing a repo or vault and wants to understand what it found.
- **Developer in task mode:** Needs to find relevant files, symbols, dependencies, and notes for a change.
- **Maintainer:** Wants to spot hubs, architecture bridges, stale docs, unlinked notes, test gaps, and risky change areas.
- **Agent-tool power user:** Wants the UI to explain and validate the same graph intelligence exposed through CLI and MCP tools.

## Jobs To Support

The first version should optimize for these jobs:

- "Show me the important parts of this repo or vault."
- "Where should I start reading?"
- "What depends on this?"
- "What code and notes are connected?"
- "What changed recently and what might it affect?"
- "Where are documentation, test, or knowledge gaps?"
- "How do I narrow this graph without learning a mode taxonomy?"

## Layout Direction

Do not treat the current three-pane structure as fixed. The first implementation can reuse existing panels where practical, but the spec should push toward a fresh layout that feels less like a generic graph library demo and more like an intelligence workspace.

The layout should prioritize:

- a large, fluid graph canvas as the primary surface,
- contextual controls that appear where they are needed,
- guidance that does not permanently consume too much space,
- detail panels that can dock, float, collapse, or become bottom sheets depending on task and viewport,
- smooth movement between overview, local exploration, impact, and detail reading.

Recommended exploration paths:

- **Map With Command Shelf:** a full-bleed graph canvas with a compact floating command shelf for Start Here, search, filters, and perspectives.
- **Constellation Workbench:** a graph-centered layout where selected nodes open small contextual satellites for actions, detail previews, related notes, and impact.
- **Narrated Map:** a graph canvas paired with a lightweight insight strip that explains the current scene and recommends the next few moves.

These directions are not mutually exclusive. The implementation can combine them, but it should avoid defaulting to permanent sidebars unless they clearly serve the current task.

## Proposed First-Open Experience

### Left Rail: Start Here

Replace or augment the passive explorer-first panel with action-oriented guidance. This guidance can be a left rail, floating command shelf, collapsible drawer, bottom sheet, or other layout treatment that preserves the graph as the primary canvas.

Primary sections:

- **Overview:** top hubs, architectural bridges, entry points, repositories, vaults, high-rank notes.
- **Recent:** recently changed files/symbols, recent graph updates, recent indexing activity.
- **Gaps:** undocumented modules, untested entry points, orphan notes, broken note links, unlinked mentions.
- **Saved Perspectives:** user-defined or built-in graph presets.

Each item should have a one-line reason, not just a label. Example: `PaymentService - high fan-in; 28 callers`.

### Center: Overview Graph

Open with a populated graph built from a bounded overview query rather than an empty seeded context query.

Initial graph contents should be intentionally small and ranked:

- top PageRank symbols and notes,
- top hubs by degree,
- bridge nodes by betweenness or existing bridge analysis,
- repositories, vaults, and services,
- current project scope if an instance config is active,
- recent-change nodes when available.

The primary graph surface should answer, "What are the major landmarks?" It should not attempt to render the entire graph by default.

### Context Surface: Why This Matters

When nothing is selected, the context surface should summarize the current overview:

- indexed counts,
- most important clusters,
- detected gaps,
- recommended first actions.

When a node is selected, the context surface should explain:

- what the node is,
- why it appears in the overview,
- where it lives,
- what connects to it,
- what to do next.

The context surface can be a docked panel, floating panel, inline popover, bottom tray, or split reading mode. The user should feel like details are attached to the current map state, not like they have entered a separate app.

## Interaction Model

### Contextual Next Actions

Every selected node should expose a consistent action set:

- **Explore neighborhood:** show a local graph around this node.
- **Impact:** show dependents and blast radius.
- **Open source/note:** open source preview or markdown preview.
- **Related notes/code:** cross-domain context.
- **Find path:** choose another target and compute path.
- **Compare context:** compare current context against another seed or snapshot.
- **Ask about this:** prefill the LLM query bar with the selected node as context.

Actions should be buttons with icons and labels where space allows. Toolbar-only controls can remain for expert use, but they should not be the only path.

### Task-First Perspectives

Replace mode-first mental load with task-first perspective names. Internally these can map to existing graph modes.

Built-in perspectives:

- **Overview:** default guided map.
- **Local Context:** neighborhood around a selected item.
- **Impact:** what a change can affect.
- **Architecture:** services, repos, hubs, bridges, entry points.
- **Knowledge:** notes, tags, backlinks, unlinked mentions, code-note links.
- **Gaps:** docs, tests, orphan notes, broken links.
- **Timeline:** recent changes and historical snapshots.

### Progressive Control Disclosure

Group controls into menus rather than a vertical strip of unexplained buttons.

- **View:** graph/list/matrix, labels, minimap, reduced effects, theme.
- **Group:** community detection, tags, repositories, vaults, file directories.
- **Filter:** node types, edge types, scope, text query.
- **Analyze:** impact, gaps, paths, compare, semantic layout.
- **Export:** PNG, GraphML, Mermaid, JSON.

Keep keyboard shortcuts, but reveal them in tooltips and command-palette results.

### Search As Scene Builder

Search should do more than select one node.

When the user searches:

- show grouped results across symbols, files, notes, tags, projects, and commands,
- explain why each result matched,
- let the user choose `Open detail`, `Explore graph`, or `Add to current scene`,
- allow natural task phrases such as `show docs gaps`, `impact of parser`, or `notes linked to indexer`.

## Visual Requirements

- Default graph must render meaningful labels for top landmarks without overcrowding.
- Node size should encode importance in the active perspective.
- Color should encode kind or grouping, but the active legend must be visible and understandable.
- Hover should reveal label, kind, location, and top reason.
- Selection should visibly dim unrelated nodes and highlight relevant edges.
- Local graph expansion should animate or otherwise preserve orientation.
- Dense graph states should offer list or matrix alternatives instead of forcing node-link reading.

## Look And Feel Requirements

Keep the current color system, including the dark graph background and node-kind colors, but improve the interface around it.

### Graph Feel

- Panning and zooming should feel smooth, direct, and responsive.
- Force layout should settle gracefully instead of appearing jittery or arbitrary.
- Node hover, selection, and expansion should use short transitions that preserve spatial orientation.
- Local graph changes should avoid abrupt full-scene resets when the user is expanding from an existing node.
- Reduced motion must remain available and should disable decorative motion without making the graph feel broken.

### Visual Hierarchy

- The graph should have one clear focal point on first open: the most important cluster, selected Start Here item, or ranked overview center.
- Top landmarks should receive readable labels earlier than low-rank nodes.
- Secondary nodes and unrelated edges should recede through opacity and weight rather than competing equally.
- Empty space should be intentional: enough room to pan and inspect, but not so much that the graph feels lost.

### Controls And Surfaces

- Controls should feel integrated into the graph canvas instead of like debugging switches.
- Replace single-character controls with recognizable icons, labels, grouped menus, or segmented controls.
- Use tooltips for expert actions, but do not rely on tooltips to explain the primary workflow.
- Cards and panels should stay compact and operational; avoid marketing-style hero composition.
- Preserve the current restrained product-tool tone while increasing polish in spacing, typography scale, and hover/focus states.

### Labeling And Microcopy

- Labels should use plain language: `Explore neighborhood`, `Impact`, `Related notes`, not internal mode names where the user is choosing an action.
- Empty states should recommend specific next actions based on available data.
- Each Start Here item should explain why it is being shown.
- Status text should communicate what the graph is doing: loading overview, settling layout, filtered to notes, showing local context, and so on.

## Data Requirements

The overview needs an API or client-side query that returns ranked overview landmarks. It can be composed from existing data at first.

Candidate inputs:

- PageRank from the graph store,
- hubs analysis,
- bridges analysis,
- repositories and services,
- vaults, notes, tags, backlinks, and unlinked mentions,
- gap analysis,
- recent graph update metadata,
- current selected project or scope filter.

The response should include a reason string or reason codes for each recommended landmark. The UI should not invent importance without data.

## Non-Goals For First Version

- Rendering the entire graph by default.
- Building a full onboarding tour.
- Replacing CLI or MCP workflows.
- Designing a new visual brand.
- Adding collaboration or cloud features.
- Making every advanced graph algorithm visible on the first screen.

## Acceptance Criteria

### First Open

- Opening the UI with indexed data shows a populated overview without requiring search.
- If no indexed data exists, the UI shows explicit setup steps and a retry action.
- The default detail panel explains the current overview and offers at least three useful next actions.

### Navigation

- A new user can answer "where should I start?" without knowing graph modes.
- Selecting a node exposes local context, impact, source/note detail, and related code/notes actions.
- The existing search box can create or update an explorable scene.

### Controls

- Cryptic toolbar labels are replaced or grouped behind labeled controls.
- Filters and legends are understandable without reading docs.
- Expert shortcuts remain available.

### Look And Feel

- The graph keeps the existing NestWeaver color palette.
- First-open graph motion is smooth and settles without distracting jitter.
- Panning, zooming, hovering, selecting, and expanding nodes feel responsive.
- Primary landmarks have readable labels without flooding the canvas.
- The UI has a clear focal point and obvious next action in the first viewport.
- Panels, controls, and graph overlays feel like one product surface rather than separate debug tools.
- The implementation explores at least one layout that is not a permanent left-sidebar, center-graph, right-inspector frame.
- Guidance and details can collapse, float, or move so the graph remains the primary spatial surface.
- The layout feels specific to NestWeaver's code-plus-knowledge map rather than interchangeable with a generic graph visualization library.

### Accessibility

- All primary actions are keyboard reachable.
- Graph alternatives exist for screen readers and dense graphs.
- Buttons have accessible labels and visible focus states.
- Reduced motion remains supported.

### Performance

- First overview should be bounded and fast enough for immediate interaction.
- Large graphs should degrade into ranked subsets, list views, or matrix/table views rather than freezing the canvas.
- The overview query should be cacheable and compatible with live re-index updates.

## Phased Delivery

### Phase 1: Guided Overview Skeleton

- Add an Overview perspective.
- Load a bounded overview graph by default.
- Replace empty detail state with overview summary and recommended actions.
- Add Start Here rail with top hubs, repos/vaults, and gaps using existing APIs where possible.
- Prototype one nonstandard layout treatment for Start Here and context details, such as a floating command shelf, contextual node satellites, or collapsible insight strip.

### Phase 2: Contextual Actions

- Add node action buttons to the detail panel.
- Map actions to existing context, impact, local, source, note, path, diff, and LLM flows.
- Improve search result actions so search can build a scene.

### Phase 3: Control Reorganization

- Replace the vertical cryptic toolbar with grouped controls.
- Add readable legends and active filter summaries.
- Preserve shortcuts and compact expert access.
- Re-evaluate whether persistent side panels are still needed after grouped controls and contextual actions are in place.

### Phase 4: Dense Graph Alternatives

- Improve list view as a first-class ranked table.
- Add matrix/table mode for dense dependency or backlink views.
- Add perspective-specific visual encodings and legends.

## Open Questions

- Which existing analysis endpoint is the best source for initial bridge nodes?
- Should the Start Here rail replace ExplorerPanel, or should it become a new first tab before Files, Symbols, and Notes?
- How much should the LLM query bar participate in first-open guidance versus staying as an optional expert tool?
- Should saved perspectives be persisted locally in Zustand only, or in the instance database for sharing across tools?

## Success Metrics

- A first-time user can identify three important landmarks in under 60 seconds.
- A user can move from first open to a local context graph in one click.
- A user can discover impact, gaps, and related notes without reading docs.
- First overview render remains responsive on large indexed projects by using bounded ranked subsets.
- User testing shows reduced confusion around graph modes and toolbar controls.

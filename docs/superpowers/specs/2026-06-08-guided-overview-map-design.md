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

## Proposed First-Open Layout

The default screen should keep the current three-region structure, but change what each region does.

### Left Rail: Start Here

Replace or augment the passive explorer-first panel with an action-oriented rail.

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

The center graph should answer, "What are the major landmarks?" It should not attempt to render the entire graph by default.

### Right Panel: Why This Matters

When nothing is selected, the detail panel should summarize the current overview:

- indexed counts,
- most important clusters,
- detected gaps,
- recommended first actions.

When a node is selected, the panel should explain:

- what the node is,
- why it appears in the overview,
- where it lives,
- what connects to it,
- what to do next.

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

### Phase 2: Contextual Actions

- Add node action buttons to the detail panel.
- Map actions to existing context, impact, local, source, note, path, diff, and LLM flows.
- Improve search result actions so search can build a scene.

### Phase 3: Control Reorganization

- Replace the vertical cryptic toolbar with grouped controls.
- Add readable legends and active filter summaries.
- Preserve shortcuts and compact expert access.

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

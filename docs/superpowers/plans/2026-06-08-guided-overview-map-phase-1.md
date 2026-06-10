# Guided Overview Map Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first usable Guided Overview Map slice: the UI opens to a populated Overview perspective with a bounded graph, a fresh command-shelf layout prototype, and useful overview/context guidance.

**Architecture:** Add a small `/api/v1/overview` endpoint that composes existing store data into ranked landmarks, then add an `overview` graph mode in the React app. Keep existing graph rendering and mode hooks, but introduce a nonstandard overlay layout for Phase 1: a full graph canvas with a floating command shelf and compact context surface instead of relying only on permanent side panels.

**Tech Stack:** Rust/Axum backend, `nestweaver_store` and `nestweaver_engine` APIs, React 19, Zustand, Graphology, Three.js/R3F, Tailwind CSS, Playwright.

---

## Scope

This plan implements Phase 1 from `docs/superpowers/specs/2026-06-08-guided-overview-map-design.md`.

In scope:

- backend overview endpoint,
- frontend overview API types,
- overview graph builder,
- `overview` graph mode and default mode,
- floating Start Here command shelf,
- overview context surface,
- first-open and endpoint tests.

Out of scope:

- full toolbar redesign,
- graph matrix/table mode,
- saved perspectives persistence,
- advanced motion tuning beyond preserving existing graph behavior,
- full visual redesign.

## File Structure

- Create `crates/nestweaver-web/src/routes/overview.rs`: compose ranked overview data from existing store/engine queries.
- Modify `crates/nestweaver-web/src/routes/mod.rs`: expose the new route module.
- Modify `crates/nestweaver-web/src/lib.rs`: wire `GET /api/v1/overview`.
- Modify `crates/nestweaver-web/tests/api_test.rs`: verify populated overview data on the in-memory fixture.
- Modify `crates/nestweaver-web/tests/smoke_test.rs`: verify the endpoint responds on an empty store.
- Modify `crates/nestweaver-web/frontend/src/api/types.ts`: add overview DTOs and `overview` graph mode.
- Modify `crates/nestweaver-web/frontend/src/api/client.ts`: add `api.overview()`.
- Create `crates/nestweaver-web/frontend/src/components/graph/utils/buildGraphFromOverview.ts`: convert overview DTOs into graphology nodes.
- Create `crates/nestweaver-web/frontend/src/components/graph/modes/useOverviewMode.ts`: load overview data and graph.
- Modify `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx`: run overview mode and render overview overlays.
- Modify `crates/nestweaver-web/frontend/src/stores/graphSlice.ts`: default graph mode to `overview`.
- Modify `crates/nestweaver-web/frontend/src/components/graph/ModeTabs.tsx`: add Overview tab.
- Create `crates/nestweaver-web/frontend/src/components/overview/OverviewCommandShelf.tsx`: floating Start Here prototype.
- Create `crates/nestweaver-web/frontend/src/components/overview/OverviewContextSurface.tsx`: compact overview/selection guidance.
- Modify `crates/nestweaver-web/frontend/e2e/graph-explorer.spec.ts`: assert first-open overview UI.

---

### Task 1: Add Backend Overview Endpoint

**Files:**
- Create: `crates/nestweaver-web/src/routes/overview.rs`
- Modify: `crates/nestweaver-web/src/routes/mod.rs`
- Modify: `crates/nestweaver-web/src/lib.rs`
- Test: `crates/nestweaver-web/tests/api_test.rs`
- Test: `crates/nestweaver-web/tests/smoke_test.rs`

- [ ] **Step 1: Write failing API tests**

Append to `crates/nestweaver-web/tests/api_test.rs`:

```rust
#[tokio::test]
async fn overview_returns_ranked_landmarks() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/overview?limit=10").await;
    assert_eq!(status, StatusCode::OK);

    assert!(json.get("counts").is_some(), "should include counts");
    assert_eq!(json["counts"]["repo_count"], 1);
    assert_eq!(json["counts"]["symbol_count"], 1);

    let landmarks = json["landmarks"]
        .as_array()
        .expect("landmarks should be an array");
    assert!(
        landmarks.iter().any(|item| item["uid"] == "sym:test:greet"),
        "overview should include top symbol"
    );

    let start_here = json["start_here"]
        .as_array()
        .expect("start_here should be an array");
    assert!(
        start_here.iter().any(|item| item["kind"] == "symbol"),
        "start_here should include symbol guidance"
    );
}
```

In `crates/nestweaver-web/tests/smoke_test.rs`, add this assertion after the health check:

```rust
assert_eq!(
    check(&app, Method::GET, "/api/v1/overview", None).await,
    StatusCode::OK
);
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p nestweaver-web overview_returns_ranked_landmarks
cargo test -p nestweaver-web all_endpoints_respond
```

Expected: fail because `/api/v1/overview` is not routed yet.

- [ ] **Step 3: Implement route module**

Create `crates/nestweaver-web/src/routes/overview.rs`:

```rust
use std::cmp::Ordering;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct OverviewParams {
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct OverviewCounts {
    repo_count: usize,
    service_count: usize,
    vault_count: usize,
    note_count: usize,
    symbol_count: usize,
    gap_count: usize,
}

#[derive(Clone, Serialize)]
struct OverviewLandmark {
    uid: String,
    kind: String,
    label: String,
    location: String,
    score: f64,
    reason: String,
}

#[derive(Serialize)]
struct OverviewGap {
    kind: String,
    label: String,
    detail: String,
}

#[derive(Serialize)]
struct OverviewResponse {
    counts: OverviewCounts,
    landmarks: Vec<OverviewLandmark>,
    start_here: Vec<OverviewLandmark>,
    gaps: Vec<OverviewGap>,
}

pub async fn overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<OverviewParams>,
) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(24).clamp(6, 100);

    let repos = nestweaver_engine::list_repos(&state.store, None)?;
    let services = nestweaver_engine::list_services(&state.store, None)?;
    let symbols = state.store.symbols_by_pagerank(Some(limit))?;
    let vaults = state.store.list_vaults(None)?;
    let mut notes = state.store.list_notes(None)?;

    notes.sort_by(|a, b| {
        b.pagerank_score
            .partial_cmp(&a.pagerank_score)
            .unwrap_or(Ordering::Equal)
    });
    notes.truncate(limit.min(12));

    let refs_count = state.store.count_references_code_edges()?;
    let mut gaps = Vec::new();
    if refs_count == 0 && !symbols.is_empty() {
        gaps.push(OverviewGap {
            kind: "documentation".to_string(),
            label: "Code has no note links".to_string(),
            detail: "No notes currently reference code symbols.".to_string(),
        });
    }

    let mut landmarks = Vec::new();

    for repo in &repos {
        landmarks.push(OverviewLandmark {
            uid: repo.uid.clone(),
            kind: "repo".to_string(),
            label: repo.name.clone().unwrap_or_else(|| {
                repo.url
                    .rsplit('/')
                    .next()
                    .unwrap_or(repo.uid.as_str())
                    .to_string()
            }),
            location: repo.url.clone(),
            score: 1.0,
            reason: "Indexed repository".to_string(),
        });
    }

    for service in &services {
        landmarks.push(OverviewLandmark {
            uid: service.uid.clone(),
            kind: "service".to_string(),
            label: service.name.clone(),
            location: service.repo_uid.clone(),
            score: 0.9,
            reason: "Detected service".to_string(),
        });
    }

    for symbol in &symbols {
        landmarks.push(OverviewLandmark {
            uid: symbol.uid.clone(),
            kind: format!("{:?}", symbol.kind),
            label: symbol.name.clone(),
            location: symbol.file_path.clone(),
            score: symbol.pagerank_score.unwrap_or(0.0),
            reason: "High PageRank symbol".to_string(),
        });
    }

    for note in &notes {
        landmarks.push(OverviewLandmark {
            uid: note.uid.clone(),
            kind: "note".to_string(),
            label: note.title.clone(),
            location: note.file_path.clone(),
            score: note.pagerank_score,
            reason: "High PageRank note".to_string(),
        });
    }

    landmarks.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    landmarks.truncate(limit);

    let start_here = landmarks.iter().take(8).cloned().collect();

    Ok(Json(OverviewResponse {
        counts: OverviewCounts {
            repo_count: repos.len(),
            service_count: services.len(),
            vault_count: vaults.len(),
            note_count: state.store.count_notes()?,
            symbol_count: state.store.symbols_by_pagerank(Some(1000))?.len(),
            gap_count: gaps.len(),
        },
        landmarks,
        start_here,
        gaps,
    })
    .into_response())
}
```

- [ ] **Step 4: Wire route module**

Modify `crates/nestweaver-web/src/routes/mod.rs`:

```rust
pub mod overview;
```

Modify `crates/nestweaver-web/src/lib.rs` by adding the route after version:

```rust
.route("/api/v1/overview", get(routes::overview::overview))
```

- [ ] **Step 5: Run backend tests to verify they pass**

Run:

```bash
cargo test -p nestweaver-web overview_returns_ranked_landmarks
cargo test -p nestweaver-web all_endpoints_respond
```

Expected: both tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-web/src/routes/overview.rs crates/nestweaver-web/src/routes/mod.rs crates/nestweaver-web/src/lib.rs crates/nestweaver-web/tests/api_test.rs crates/nestweaver-web/tests/smoke_test.rs
git commit -m "feat(web): add overview API"
```

---

### Task 2: Add Frontend Overview Types And Graph Builder

**Files:**
- Modify: `crates/nestweaver-web/frontend/src/api/types.ts`
- Modify: `crates/nestweaver-web/frontend/src/api/client.ts`
- Create: `crates/nestweaver-web/frontend/src/components/graph/utils/buildGraphFromOverview.ts`

- [ ] **Step 1: Add frontend DTOs and graph mode**

Modify `crates/nestweaver-web/frontend/src/api/types.ts`.

Add:

```ts
export interface OverviewCounts {
  repo_count: number;
  service_count: number;
  vault_count: number;
  note_count: number;
  symbol_count: number;
  gap_count: number;
}

export interface OverviewLandmark {
  uid: string;
  kind: string;
  label: string;
  location: string;
  score: number;
  reason: string;
}

export interface OverviewGap {
  kind: string;
  label: string;
  detail: string;
}

export interface OverviewResponse {
  counts: OverviewCounts;
  landmarks: OverviewLandmark[];
  start_here: OverviewLandmark[];
  gaps: OverviewGap[];
}
```

Update `GraphMode`:

```ts
export type GraphMode =
  | "overview"
  | "context"
  | "impact"
  | "repos"
  | "features"
  | "local";
```

- [ ] **Step 2: Add client method**

Modify `crates/nestweaver-web/frontend/src/api/client.ts`.

Add `OverviewResponse` to the type import list:

```ts
OverviewResponse,
```

Add this method near `brainContext`:

```ts
overview(limit = 24) {
  return get<OverviewResponse>(`/api/v1/overview?limit=${limit}`);
},
```

- [ ] **Step 3: Create overview graph builder**

Create `crates/nestweaver-web/frontend/src/components/graph/utils/buildGraphFromOverview.ts`:

```ts
import Graph from "graphology";
import type { OverviewResponse, OverviewLandmark } from "../../../api/types";
import { kindToColor, nodeSize } from "./graphColors";

function landmarkColor(item: OverviewLandmark): string {
  if (item.kind === "repo") return "#6B7280";
  if (item.kind === "service") return "#3B82F6";
  if (item.kind === "note") return "#78716C";
  return kindToColor(item.kind);
}

export function buildGraphFromOverview(result: OverviewResponse): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const maxScore = Math.max(...result.landmarks.map((n) => n.score), 0.001);

  for (let i = 0; i < result.landmarks.length; i++) {
    const item = result.landmarks[i];
    const angle = (i / Math.max(result.landmarks.length, 1)) * Math.PI * 2;
    const ring = item.kind === "repo" || item.kind === "service" ? 220 : 120;
    const normalized = Math.max(item.score / maxScore, 0.08);

    graph.addNode(item.uid, {
      label: item.label,
      x: Math.cos(angle) * ring,
      y: Math.sin(angle) * ring,
      size: nodeSize(1, normalized),
      color: landmarkColor(item),
      kind: item.kind,
      location: item.location,
      relevance: item.score,
      reason: item.reason,
      forceLabel: i < 8,
      isOverview: true,
    });
  }

  return graph;
}
```

- [ ] **Step 4: Run frontend type check**

Run:

```bash
cd crates/nestweaver-web/frontend && npm run build
```

Expected: fail if `overview` graph mode is not yet handled everywhere. This is acceptable at this step; record the first TypeScript errors for Task 3.

- [ ] **Step 5: Commit if build only fails on missing mode handling**

```bash
git add crates/nestweaver-web/frontend/src/api/types.ts crates/nestweaver-web/frontend/src/api/client.ts crates/nestweaver-web/frontend/src/components/graph/utils/buildGraphFromOverview.ts
git commit -m "feat(web): add overview frontend types"
```

---

### Task 3: Make Overview The Default Graph Mode

**Files:**
- Modify: `crates/nestweaver-web/frontend/src/stores/graphSlice.ts`
- Modify: `crates/nestweaver-web/frontend/src/components/graph/ModeTabs.tsx`
- Modify: `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx`
- Create: `crates/nestweaver-web/frontend/src/components/graph/modes/useOverviewMode.ts`

- [ ] **Step 1: Create overview mode hook**

Create `crates/nestweaver-web/frontend/src/components/graph/modes/useOverviewMode.ts`:

```ts
import { useCallback, useEffect, useState } from "react";
import { api } from "../../../api/client";
import type { OverviewResponse } from "../../../api/types";
import { useStore } from "../../../stores";
import { buildGraphFromOverview } from "../utils/buildGraphFromOverview";

export function useOverviewMode() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphData = useStore((s) => s.setGraphData);
  const [overview, setOverview] = useState<OverviewResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadOverview = useCallback(async () => {
    if (graphMode !== "overview") return;
    setLoading(true);
    setError(null);
    try {
      const result = await api.overview(24);
      setOverview(result);
      setGraphData(buildGraphFromOverview(result));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load overview");
    } finally {
      setLoading(false);
    }
  }, [graphMode, setGraphData]);

  useEffect(() => {
    loadOverview();
  }, [loadOverview]);

  return { overview, loading, error, reload: loadOverview };
}
```

- [ ] **Step 2: Default store to overview**

Modify `crates/nestweaver-web/frontend/src/stores/graphSlice.ts`:

```ts
graphMode: "overview",
```

- [ ] **Step 3: Add Overview mode tab**

Modify `crates/nestweaver-web/frontend/src/components/graph/ModeTabs.tsx`:

```ts
const modes: { key: GraphMode; label: string }[] = [
  { key: "overview", label: "Overview" },
  { key: "context", label: "Context" },
  { key: "impact", label: "Impact" },
  { key: "repos", label: "Repos" },
  { key: "features", label: "Features" },
  { key: "local", label: "Local" },
];
```

- [ ] **Step 4: Run overview hook in graph mode hooks**

Modify `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx`.

Add import:

```ts
import { useOverviewMode } from "./modes/useOverviewMode";
```

Inside `GraphModeHooks()` add:

```ts
useOverviewMode();
```

- [ ] **Step 5: Run frontend build**

Run:

```bash
cd crates/nestweaver-web/frontend && npm run build
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-web/frontend/src/stores/graphSlice.ts crates/nestweaver-web/frontend/src/components/graph/ModeTabs.tsx crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx crates/nestweaver-web/frontend/src/components/graph/modes/useOverviewMode.ts
git commit -m "feat(web): load overview graph by default"
```

---

### Task 4: Prototype Fresh Overview Layout

**Files:**
- Create: `crates/nestweaver-web/frontend/src/components/overview/OverviewCommandShelf.tsx`
- Create: `crates/nestweaver-web/frontend/src/components/overview/OverviewContextSurface.tsx`
- Modify: `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx`

- [ ] **Step 1: Create command shelf component**

Create `crates/nestweaver-web/frontend/src/components/overview/OverviewCommandShelf.tsx`:

```tsx
import type { OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";

interface OverviewCommandShelfProps {
  overview: OverviewResponse | null;
  loading: boolean;
  error: string | null;
  onReload: () => void;
}

export function OverviewCommandShelf({
  overview,
  loading,
  error,
  onReload,
}: OverviewCommandShelfProps) {
  const selectNode = useStore((s) => s.selectNode);

  return (
    <div className="absolute left-4 top-4 z-20 w-[min(340px,calc(100vw-2rem))] rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)]/95 p-3 shadow-xl backdrop-blur">
      <div className="mb-2 flex items-center justify-between gap-2">
        <div>
          <h2 className="text-sm font-semibold text-[var(--color-text)]">Start Here</h2>
          <p className="text-xs text-[var(--color-text-muted)]">
            Ranked landmarks from the current index.
          </p>
        </div>
        <button
          type="button"
          onClick={onReload}
          className="rounded border border-[var(--color-border)] px-2 py-1 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
        >
          Refresh
        </button>
      </div>

      {loading && <p className="text-xs text-[var(--color-text-muted)]">Loading overview...</p>}
      {error && <p className="text-xs text-red-500">{error}</p>}

      {!loading && !error && overview && (
        <div className="space-y-1.5">
          {overview.start_here.slice(0, 6).map((item) => (
            <button
              key={item.uid}
              type="button"
              onClick={() => selectNode(item.uid, item.kind)}
              className="w-full rounded border border-transparent px-2 py-1.5 text-left hover:border-[var(--color-border)] hover:bg-[var(--color-surface-alt)]"
            >
              <div className="flex items-center gap-2">
                <span className="text-[10px] uppercase text-[var(--color-text-muted)]">
                  {item.kind}
                </span>
                <span className="truncate text-xs font-medium text-[var(--color-text)]">
                  {item.label}
                </span>
              </div>
              <p className="truncate text-[11px] text-[var(--color-text-muted)]">
                {item.reason}
              </p>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Create context surface component**

Create `crates/nestweaver-web/frontend/src/components/overview/OverviewContextSurface.tsx`:

```tsx
import type { OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";

interface OverviewContextSurfaceProps {
  overview: OverviewResponse | null;
}

export function OverviewContextSurface({ overview }: OverviewContextSurfaceProps) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const graphInstance = useStore((s) => s.graphInstance);
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);

  const selected =
    selectedNodeId && graphInstance?.hasNode(selectedNodeId)
      ? {
          label: graphInstance.getNodeAttribute(selectedNodeId, "label") as string,
          kind: graphInstance.getNodeAttribute(selectedNodeId, "kind") as string,
          reason: graphInstance.getNodeAttribute(selectedNodeId, "reason") as string | undefined,
          location: graphInstance.getNodeAttribute(selectedNodeId, "location") as string | undefined,
        }
      : null;

  return (
    <div className="absolute bottom-4 right-4 z-20 w-[min(360px,calc(100vw-2rem))] rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)]/95 p-3 shadow-xl backdrop-blur">
      {selectedNodeId && selected ? (
        <>
          <p className="text-[10px] uppercase text-[var(--color-text-muted)]">{selected.kind}</p>
          <h2 className="mt-0.5 truncate text-sm font-semibold text-[var(--color-text)]">
            {selected.label}
          </h2>
          <p className="mt-1 text-xs text-[var(--color-text-muted)]">
            {selected.reason ?? "Selected overview landmark"}
          </p>
          {selected.location && (
            <p className="mt-1 truncate text-[11px] text-[var(--color-text-muted)]">
              {selected.location}
            </p>
          )}
          <div className="mt-3 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => {
                setSeeds([selectedNodeId]);
                setGraphMode("local");
              }}
              className="rounded bg-blue-600 px-2 py-1 text-xs font-medium text-white hover:bg-blue-500"
            >
              Explore neighborhood
            </button>
            <button
              type="button"
              onClick={() => setGraphMode("impact")}
              className="rounded border border-[var(--color-border)] px-2 py-1 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
            >
              Impact
            </button>
          </div>
        </>
      ) : (
        <>
          <h2 className="text-sm font-semibold text-[var(--color-text)]">Overview Map</h2>
          <p className="mt-1 text-xs text-[var(--color-text-muted)]">
            {overview
              ? `${overview.counts.repo_count} repos, ${overview.counts.symbol_count} symbols, ${overview.counts.note_count} notes.`
              : "Open an indexed project to see ranked landmarks."}
          </p>
          {overview && overview.gaps.length > 0 && (
            <p className="mt-2 text-xs text-amber-600">{overview.gaps[0].detail}</p>
          )}
        </>
      )}
    </div>
  );
}
```

- [ ] **Step 3: Expose overview hook state to GraphPanel**

Modify `GraphModeHooks()` in `crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx` to return overview state:

```tsx
function GraphModeHooks() {
  const overviewState = useOverviewMode();
  useContextMode();
  useImpactMode();
  useReposMode();
  useFeaturesMode();
  const { hops, setHops } = useLocalMode();
  ...
  return (
    <>
      {graphMode === "overview" && (
        <>
          <OverviewCommandShelf {...overviewState} />
          <OverviewContextSurface overview={overviewState.overview} />
        </>
      )}
      {graphMode === "local" && (
        ...
      )}
    </>
  );
}
```

Add imports:

```ts
import { OverviewCommandShelf } from "../overview/OverviewCommandShelf";
import { OverviewContextSurface } from "../overview/OverviewContextSurface";
```

- [ ] **Step 4: Run frontend build**

Run:

```bash
cd crates/nestweaver-web/frontend && npm run build
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/nestweaver-web/frontend/src/components/overview/OverviewCommandShelf.tsx crates/nestweaver-web/frontend/src/components/overview/OverviewContextSurface.tsx crates/nestweaver-web/frontend/src/components/graph/GraphPanel.tsx
git commit -m "feat(web): prototype overview command shelf"
```

---

### Task 5: Add E2E Coverage For First-Open Overview

**Files:**
- Modify: `crates/nestweaver-web/frontend/e2e/graph-explorer.spec.ts`

- [ ] **Step 1: Write failing Playwright assertions**

Modify the first test in `crates/nestweaver-web/frontend/e2e/graph-explorer.spec.ts`:

```ts
test("overview opens with guidance", async ({ page }) => {
  await page.goto("/");
  const graphContainer = page.locator('[data-testid="graph-panel"]');
  await expect(graphContainer).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText("Start Here")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText("Overview Map")).toBeVisible({ timeout: 15_000 });
});
```

- [ ] **Step 2: Run frontend build and e2e**

Run:

```bash
cd crates/nestweaver-web/frontend && npm run build
```

Expected: pass.

Run:

```bash
cd crates/nestweaver-web/frontend && npm run test:e2e -- graph-explorer.spec.ts
```

Expected: pass when the Playwright backend fixture is healthy. If the local Playwright setup lacks a seeded database, record the failure and run the backend endpoint tests plus frontend build as the minimum verification.

- [ ] **Step 3: Commit**

```bash
git add crates/nestweaver-web/frontend/e2e/graph-explorer.spec.ts
git commit -m "test(web): cover first-open overview"
```

---

### Task 6: Final Verification

**Files:**
- No source changes expected.

- [ ] **Step 1: Run backend tests**

```bash
cargo test -p nestweaver-web
```

Expected: pass.

- [ ] **Step 2: Run frontend build**

```bash
cd crates/nestweaver-web/frontend && npm run build
```

Expected: pass.

- [ ] **Step 3: Run focused e2e**

```bash
cd crates/nestweaver-web/frontend && npm run test:e2e -- graph-explorer.spec.ts
```

Expected: pass, or document the exact fixture/environment blocker.

- [ ] **Step 4: Review spec coverage**

Confirm Phase 1 satisfies:

- default populated overview,
- one nonstandard layout treatment,
- Start Here guidance,
- contextual selected-node actions,
- first-open tests,
- unchanged NestWeaver color system.

- [ ] **Step 5: Confirm no unexpected changes remain**

```bash
git status --short
```

Expected: no output. If there is output, stop and inspect `git diff`; do not stage or commit additional files from this final verification step without a fresh review.

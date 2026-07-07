import { api, loadGapItems } from "../api/client";
import type { SceneMetadata } from "../api/p1Types";
import type { StoreState } from "../stores";
import { phraseCandidateToTarget } from "./resolve";
import type {
  PhraseCandidate,
  PhraseCandidateOverrides,
  PhraseExecutionResult,
  PhraseResolution,
  PhraseResolvedTarget,
  PhraseTargetRole,
} from "./types";

interface ExecutePhraseOptions {
  targetOverride?: PhraseCandidate;
  targetOverrides?: PhraseCandidateOverrides;
  isCurrent?: () => boolean;
  getCurrentState?: () => StoreState;
}

function targetFromResolution(
  resolution: PhraseResolution,
  override?: PhraseCandidate,
): PhraseResolvedTarget | null {
  if (override) return phraseCandidateToTarget(override);
  return resolution.targets[0] ?? null;
}

function applyMetadata(state: StoreState, metadata: SceneMetadata | null) {
  if (metadata) state.setSceneMetadata(metadata);
}

function isCurrent(options: ExecutePhraseOptions): boolean {
  return options.isCurrent?.() ?? true;
}

function currentState(state: StoreState, options: ExecutePhraseOptions): StoreState {
  return options.getCurrentState?.() ?? state;
}

function canApplyTraceResult(
  state: StoreState,
  targetUid: string,
  workspaceId: string,
): boolean {
  return (
    state.selectedNodeId === targetUid &&
    state.detailFocus === "analysis" &&
    state.activeWorkspaceId === workspaceId &&
    state.activeLens.lens === "trace" &&
    state.activeLens.targetUid === targetUid &&
    state.activeLens.workspaceId === workspaceId
  );
}

function resultForExecution(
  resolution: PhraseResolution,
): SceneMetadata["trust"]["result"] {
  if (resolution.status === "unsupported") return "unsupported";
  if (resolution.status === "ambiguous") return "ambiguous";
  if (resolution.status === "no-match") return "no-match";
  if (resolution.status === "error") return "error";
  if (resolution.supportLevel === "limited") return "partial";
  return "complete";
}

function unsupportedForExecution(resolution: PhraseResolution): string[] {
  if (resolution.supportLevel === "unsupported") return [resolution.coverage.behavior];
  if (resolution.supportLevel === "limited") return resolution.coverage.limits;
  return [];
}

function metadataForExecution(
  resolution: PhraseResolution,
  selectedTargets: PhraseResolvedTarget[] = [],
): SceneMetadata | null {
  const workspaceMetadata =
    selectedTargets.find(isWorkspaceTarget)?.metadata ?? null;
  const resolutionMetadata = resolution.metadata;
  const metadata = workspaceMetadata ?? resolutionMetadata;
  if (!metadata) return null;

  const result = resultForExecution(resolution);
  const executionMetadata = {
    ...metadata,
    trust: {
      ...metadata.trust,
      result,
      partial: metadata.trust.partial || result !== "complete",
      unsupported: Array.from(
        new Set([
          ...metadata.trust.unsupported,
          ...unsupportedForExecution(resolution),
        ]),
      ),
      message: resolutionMetadata?.trust.message ?? resolution.summary,
    },
  };

  if (resolution.status !== "ambiguous") return executionMetadata;

  const resolvedResult =
    resolution.supportLevel === "limited" ? "partial" : "complete";
  const targetLabels = selectedTargets.map((target) => target.label).join(" to ");
  return {
    ...executionMetadata,
    trust: {
      ...executionMetadata.trust,
      result: resolvedResult,
      partial:
        resolvedResult !== "complete" ||
        (executionMetadata.trust.partial &&
          executionMetadata.trust.result !== "ambiguous"),
      message: targetLabels
        ? `${resolution.coverage.behavior} Resolved target: ${targetLabels}.`
        : resolution.coverage.behavior,
    },
  };
}

function isWorkspaceTarget(target: PhraseResolvedTarget): boolean {
  return ["workspace", "repo", "project"].includes(target.targetType);
}

function workspaceId(target: PhraseResolvedTarget): string {
  return target.id;
}

function targetForRole(
  resolution: PhraseResolution,
  role: PhraseTargetRole,
  overrides?: PhraseCandidateOverrides,
): PhraseResolvedTarget | null {
  const override = overrides?.[role];
  if (override) return phraseCandidateToTarget(override);
  return resolution.targets.find((target) => target.role === role) ?? null;
}

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message
    ? error.message
    : "Path query failed.";
}

function executeExplain(
  state: StoreState,
  resolution: PhraseResolution,
  target: PhraseResolvedTarget,
): PhraseExecutionResult {
  applyMetadata(state, metadataForExecution(resolution, [target]));
  if (isWorkspaceTarget(target)) {
    state.setActiveWorkspaceId(workspaceId(target));
    state.setGraphMode("overview");
    state.setActiveLens({
      lens: "overview",
      label: `Explain ${target.label}`,
      targetUid: target.uid ?? null,
      workspaceId: workspaceId(target),
    });
    return { status: "executed", message: `Opened workspace ${target.label}.` };
  }

  state.exploreNode(target.uid ?? target.id, target.kind);
  state.setDetailFocus("summary");
  state.setActiveLens({
    lens: target.targetType === "note" ? "rationale" : "context",
    label: `Explain ${target.label}`,
    targetUid: target.uid ?? target.id,
    workspaceId: state.activeWorkspaceId,
  });
  return { status: "executed", message: `Opened ${target.label}.` };
}

async function executePath(
  state: StoreState,
  resolution: PhraseResolution,
  options: ExecutePhraseOptions,
): Promise<PhraseExecutionResult> {
  const source = targetForRole(resolution, "source", options.targetOverrides);
  const destination = targetForRole(resolution, "destination", options.targetOverrides);
  if (!source || !destination) {
    return { status: "error", message: "Path endpoints are not resolved." };
  }
  applyMetadata(state, metadataForExecution(resolution, [source, destination]));
  const from = source.uid ?? source.id;
  const to = destination.uid ?? destination.id;
  state.selectNode(from, source.kind);
  state.startPathfinding(from);
  const request = state.setPathfindingTarget(to);
  state.setDetailFocus("analysis");
  state.setActiveLens({
    lens: "path",
    label: `Path from ${source.label} to ${destination.label}`,
    targetUid: from,
    workspaceId: state.activeWorkspaceId,
  });
  let results: Awaited<ReturnType<typeof api.paths>>;
  try {
    results = await api.paths(from, to, 5, 10);
  } catch (error) {
    const message = errorMessage(error);
    if (!isCurrent(options) || !state.isCurrentPathRequest(request)) {
      return { status: "error", message: "Path result was superseded by a newer phrase." };
    }
    state.setPathError(message, request);
    return { status: "error", message };
  }
  if (!isCurrent(options) || !state.isCurrentPathRequest(request)) {
    return { status: "error", message: "Path result was superseded by a newer phrase." };
  }
  state.setPathResults(results, request);
  return {
    status: "executed",
    message: results.length > 0 ? `Found ${results.length} path result(s).` : "No path results found.",
  };
}

export async function executeSearchPhrase(
  state: StoreState,
  resolution: PhraseResolution,
  options: ExecutePhraseOptions = {},
): Promise<PhraseExecutionResult> {
  const target = targetFromResolution(resolution, options.targetOverride);
  const executionMetadata = metadataForExecution(
    resolution,
    target ? [target] : [],
  );

  if (resolution.status === "unsupported") {
    applyMetadata(state, executionMetadata);
    state.setActiveLens({
      lens: "unsupported",
      label: resolution.title,
      targetUid: null,
      workspaceId: state.activeWorkspaceId,
    });
    return { status: "unsupported", message: resolution.summary };
  }

  switch (resolution.intent.kind) {
    case "explain":
      if (!target) return { status: "error", message: "Choose a target first." };
      return executeExplain(state, resolution, target);

    case "impact":
      if (!target) return { status: "error", message: "Choose a symbol first." };
      applyMetadata(state, executionMetadata);
      state.selectNode(target.uid ?? target.id, target.kind);
      state.setGraphMode("impact");
      state.setDetailFocus("analysis");
      state.setActiveLens({
        lens: "impact",
        label: `Impact of ${target.label}`,
        targetUid: target.uid ?? target.id,
        workspaceId: state.activeWorkspaceId,
      });
      return { status: "executed", message: `Opened impact for ${target.label}.` };

    case "trace_flow":
      if (!target) return { status: "error", message: "Choose a symbol first." };
      applyMetadata(state, executionMetadata);
      const traceTargetUid = target.uid ?? target.id;
      const traceWorkspaceId = state.activeWorkspaceId;
      state.selectNode(traceTargetUid, target.kind);
      state.setDetailFocus("analysis");
      state.setActiveLens({
        lens: "trace",
        label: `Trace from ${target.label}`,
        targetUid: traceTargetUid,
        workspaceId: traceWorkspaceId,
      });
      state.clearFlowTrace();
      const result = await api.flow(traceTargetUid, 10);
      const latestState = currentState(state, options);
      if (!isCurrent(options) || !canApplyTraceResult(latestState, traceTargetUid, traceWorkspaceId)) {
        return { status: "error", message: "Trace result was superseded by a newer phrase." };
      }
      state.setFlowTrace(result);
      return { status: "executed", message: `Opened trace for ${target.label}.` };

    case "callers":
    case "callees":
      if (!target) return { status: "error", message: "Choose a symbol first." };
      applyMetadata(state, executionMetadata);
      state.selectNode(target.uid ?? target.id, target.kind);
      state.setDetailFocus("related");
      state.setGraphMode("local");
      state.setActiveLens({
        lens: "search",
        label: resolution.intent.kind === "callers" ? `Callers of ${target.label}` : `Callees of ${target.label}`,
        targetUid: target.uid ?? target.id,
        workspaceId: state.activeWorkspaceId,
      });
      return { status: "executed", message: `Opened relationships for ${target.label}.` };

    case "path":
      return executePath(state, resolution, options);

    case "tests_affected":
      if (!target) return { status: "error", message: "Choose a target first." };
      applyMetadata(state, executionMetadata);
      if (target.targetType === "symbol") {
        state.selectNode(target.uid ?? target.id, target.kind);
        state.setGraphMode("impact");
      }
      state.setDetailFocus("analysis");
      state.setActiveLens({
        lens: "impact",
        label: `Tests affected by ${target.label}`,
        targetUid: target.uid ?? target.id,
        workspaceId: state.activeWorkspaceId,
      });
      return {
        status: "limited",
        message: "Opened impact context with limited affected-test metadata.",
      };

    case "dead_code": {
      applyMetadata(state, executionMetadata);
      const lensWorkspaceId =
        target && isWorkspaceTarget(target)
          ? workspaceId(target)
          : state.activeWorkspaceId;
      if (target && isWorkspaceTarget(target)) {
        state.setActiveWorkspaceId(lensWorkspaceId);
      }
      const items = await loadGapItems();
      if (!isCurrent(options)) {
        return { status: "error", message: "Dead-code result was superseded by a newer phrase." };
      }
      state.setGapItems(items);
      if (!state.gapActive) state.toggleGapPanel();
      state.setActiveLens({
        lens: "unsupported",
        label: "Dead code proxy",
        targetUid: target?.uid ?? null,
        workspaceId: lensWorkspaceId,
      });
      return { status: "limited", message: "Opened local gap analysis as a limited dead-code proxy." };
    }

    case "bridges":
    case "hubs":
      applyMetadata(state, executionMetadata);
      if (target && isWorkspaceTarget(target)) state.setActiveWorkspaceId(workspaceId(target));
      state.setGraphMode("overview");
      state.setActiveLens({
        lens: "overview",
        label: resolution.intent.kind === "bridges" ? "Bridge hints" : "Hub hints",
        targetUid: target?.uid ?? null,
        workspaceId: target?.id ?? state.activeWorkspaceId,
      });
      return { status: "limited", message: resolution.coverage.behavior };

    case "notes_about":
      applyMetadata(state, executionMetadata);
      if (target) {
        state.exploreNode(target.uid ?? target.id, target.kind);
        state.setDetailFocus("summary");
      }
      state.setActiveLens({
        lens: "rationale",
        label: `Notes about ${resolution.intent.rawTarget ?? "topic"}`,
        targetUid: target?.uid ?? target?.id ?? null,
        workspaceId: state.activeWorkspaceId,
      });
      return { status: "executed", message: target ? `Opened ${target.label}.` : "Opened note search metadata." };

    case "backlinks":
      if (!target) return { status: "error", message: "Choose a note first." };
      applyMetadata(state, executionMetadata);
      state.selectNode(target.uid ?? target.id, target.kind);
      state.setDetailFocus("related");
      state.setActiveLens({
        lens: "rationale",
        label: `Backlinks for ${target.label}`,
        targetUid: target.uid ?? target.id,
        workspaceId: state.activeWorkspaceId,
      });
      await api.brainBacklinks(target.uid ?? target.id).catch(() => []);
      return { status: "executed", message: `Opened backlink context for ${target.label}.` };

    case "stale_repos": {
      applyMetadata(state, executionMetadata);
      const repos = await api.repos();
      if (!isCurrent(options)) {
        return { status: "error", message: "Stale repo result was superseded by a newer phrase." };
      }
      const stale = repos.filter((repo) => repo.staleness_commits_behind > 0);
      state.setActiveLens({
        lens: "freshness",
        label: "Stale repos",
        targetUid: null,
        workspaceId: state.activeWorkspaceId,
      });
      return {
        status: "limited",
        message:
          stale.length > 0
            ? `${stale.length} local repo(s) report stale commits.`
            : "No local stale repos reported by the current web API.",
      };
    }

    case "contract_drift":
      applyMetadata(state, executionMetadata);
      state.setActiveLens({
        lens: "unsupported",
        label: "Contract drift",
        targetUid: null,
        workspaceId: state.activeWorkspaceId,
      });
      return { status: "unsupported", message: resolution.summary };
  }
}

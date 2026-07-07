import type { ComponentType } from "react";
import {
  Binary,
  Bot,
  Compass,
  FileCode,
  GitCompare,
  GitFork,
  Link2,
  Network,
  Route,
  Search,
} from "lucide-react";
import { api } from "../../api/client";
import { isSymbolKind } from "../../api/kinds";
import { useStore } from "../../stores";
import type { DetailFocus } from "../../stores/graphSlice";

export type NodeActionId =
  | "open"
  | "explore"
  | "impact"
  | "related"
  | "path"
  | "compare"
  | "trace"
  | "ask"
  | "copyLink";

export interface NodeActionContext {
  uid: string;
  kind?: string | null;
  label?: string | null;
}

export interface NodeAction {
  id: NodeActionId;
  label: string;
  title: string;
  icon: ComponentType<{ className?: string }>;
  disabled?: boolean;
  disabledReason?: string;
  focus?: DetailFocus;
  run: () => void | Promise<void>;
}

function isNoteLike(uid: string, kind?: string | null): boolean {
  return kind === "note" || kind === "Note" || uid.startsWith("note:");
}

function nodeLabel(node: NodeActionContext): string {
  return node.label ?? node.uid.split(":").pop() ?? node.uid;
}

function deepLinkForNode(node: NodeActionContext): string {
  const state = useStore.getState();
  const url = new URL(window.location.href);
  const params = new URLSearchParams();

  if (state.seeds.length > 0) params.set("seeds", state.seeds.join(","));
  if (state.graphMode !== "overview") params.set("mode", state.graphMode);
  if (state.activeWorkspaceId !== "all") {
    params.set("workspace", state.activeWorkspaceId);
  }
  params.set("node", node.uid);
  if (node.kind) params.set("kind", node.kind);
  if (state.activeLens.lens !== "overview") {
    params.set("lens", state.activeLens.lens);
  }
  if (state.representationMode !== "graph") {
    params.set("representation", state.representationMode);
  }

  url.search = params.toString();
  return url.toString();
}

function clipboardUnavailable(): boolean {
  return typeof navigator === "undefined" || !navigator.clipboard;
}

let latestTraceActionId = 0;

export function useNodeActions(node: NodeActionContext | null): NodeAction[] {
  const selectNode = useStore((s) => s.selectNode);
  const openPreview = useStore((s) => s.openPreview);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setDetailFocus = useStore((s) => s.setDetailFocus);
  const startPathfinding = useStore((s) => s.startPathfinding);
  const setFlowTrace = useStore((s) => s.setFlowTrace);
  const clearFlowTrace = useStore((s) => s.clearFlowTrace);
  const startDiff = useStore((s) => s.startDiff);
  const openLlmBar = useStore((s) => s.openLlmBar);
  const setLlmQuery = useStore((s) => s.setLlmQuery);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const notify = useStore((s) => s.notify);

  if (!node) return [];

  const symbol = isSymbolKind(node.kind) || node.uid.startsWith("sym:");
  const note = isNoteLike(node.uid, node.kind);
  const label = nodeLabel(node);

  const revealDetail = (focus: DetailFocus) => {
    openPreview(node.uid, node.kind ?? null, true);
    setDetailFocus(focus);
  };

  const focusDetail = (focus: DetailFocus) => {
    selectNode(node.uid, node.kind ?? null);
    setDetailFocus(focus);
  };

  const selectForLens = (
    lens: Parameters<typeof setActiveLens>[0],
    focus: DetailFocus,
    revealPreview = false,
  ) => {
    if (revealPreview) {
      openPreview(node.uid, node.kind ?? null, true);
    } else {
      selectNode(node.uid, node.kind ?? null);
    }
    setDetailFocus(focus);
    setActiveLens(lens);
  };

  return [
    {
      id: "open",
      label: note ? "Open note" : symbol ? "Open source" : "Open detail",
      title: "Jump to the source, note, or detail preview for this item",
      icon: FileCode,
      focus: "source",
      run: () => {
        revealDetail("source");
        setActiveLens({
          lens: note ? "rationale" : "context",
          label: `Open ${label}`,
          targetUid: node.uid,
          workspaceId: activeWorkspaceId,
        });
      },
    },
    {
      id: "explore",
      label: "Explore",
      title: "Show the local neighborhood around this item",
      icon: Compass,
      run: () => {
        selectNode(node.uid, node.kind ?? null);
        setGraphMode("local");
        setDetailFocus("summary");
        setActiveLens({
          lens: "context",
          label: `Explore ${label}`,
          targetUid: node.uid,
          workspaceId: activeWorkspaceId,
        });
      },
    },
    {
      id: "impact",
      label: "Impact",
      title: symbol ? "Show dependents and blast radius" : "Impact is available for symbols",
      icon: Network,
      disabled: !symbol,
      disabledReason: symbol ? undefined : "Impact requires a symbol node.",
      focus: "analysis",
      run: () => {
        if (!symbol) return;
        selectNode(node.uid, node.kind ?? null);
        setGraphMode("impact");
        setDetailFocus("analysis");
        setActiveLens({
          lens: "impact",
          label: `Impact of ${label}`,
          targetUid: node.uid,
          workspaceId: activeWorkspaceId,
        });
      },
    },
    {
      id: "related",
      label: "Related",
      title: "Jump to related code, references, backlinks, or mentions",
      icon: Search,
      focus: "related",
      run: () => {
        focusDetail("related");
        setActiveLens({
          lens: note ? "rationale" : "search",
          label: `Related to ${label}`,
          targetUid: node.uid,
          workspaceId: activeWorkspaceId,
        });
      },
    },
    {
      id: "path",
      label: "Path",
      title: "Find a path from this item to another node",
      icon: Route,
      focus: "analysis",
      run: () => {
        selectForLens(
          {
            lens: "path",
            label: `Path from ${label}`,
            targetUid: node.uid,
            workspaceId: activeWorkspaceId,
          },
          "analysis",
          true,
        );
        startPathfinding(node.uid);
      },
    },
    {
      id: "compare",
      label: "Compare",
      title: "Compare this context with another seed set",
      icon: GitCompare,
      focus: "analysis",
      run: async () => {
        selectForLens(
          {
            lens: "context",
            label: `Compare ${label}`,
            targetUid: node.uid,
            workspaceId: activeWorkspaceId,
          },
          "analysis",
        );
        const result = await api.brainContext([node.uid], 2000, "all");
        startDiff(result, [node.uid]);
      },
    },
    {
      id: "trace",
      label: "Trace",
      title: symbol ? "Trace flow from this symbol" : "Trace is available for symbols",
      icon: GitFork,
      disabled: !symbol,
      disabledReason: symbol ? undefined : "Trace requires a symbol node.",
      focus: "analysis",
      run: async () => {
        if (!symbol) return;
        const traceActionId = ++latestTraceActionId;
        const traceTargetUid = node.uid;
        const traceWorkspaceId = activeWorkspaceId;
        selectForLens(
          {
            lens: "trace",
            label: `Trace from ${label}`,
            targetUid: traceTargetUid,
            workspaceId: traceWorkspaceId,
          },
          "analysis",
          true,
        );
        clearFlowTrace();
        const result = await api.flow(traceTargetUid, 10);
        const state = useStore.getState();
        if (
          traceActionId !== latestTraceActionId ||
          state.selectedNodeId !== traceTargetUid ||
          state.detailFocus !== "analysis" ||
          state.activeLens.lens !== "trace" ||
          state.activeLens.targetUid !== traceTargetUid ||
          state.activeLens.workspaceId !== traceWorkspaceId
        ) {
          return;
        }
        setFlowTrace(result);
      },
    },
    {
      id: "ask",
      label: "Ask",
      title: "Ask about this item with graph context",
      icon: Bot,
      run: () => {
        selectNode(node.uid, node.kind ?? null);
        setLlmQuery(`Explain ${label} and its important relationships`);
        setActiveLens({
          lens: note ? "rationale" : "context",
          label: `Ask about ${label}`,
          targetUid: node.uid,
          workspaceId: activeWorkspaceId,
        });
        openLlmBar();
      },
    },
    {
      id: "copyLink",
      label: "Copy link",
      title: clipboardUnavailable()
        ? "Copy link is unavailable because clipboard access is not supported"
        : "Copy a deep link for this workspace, node, lens, and representation",
      icon: Link2,
      disabled: clipboardUnavailable(),
      disabledReason: clipboardUnavailable()
        ? "Clipboard access is not available in this browser context."
        : undefined,
      run: async () => {
        if (clipboardUnavailable()) {
          throw new Error("Clipboard access is not available in this browser context.");
        }
        await navigator.clipboard.writeText(deepLinkForNode(node));
        notify({
          kind: "success",
          title: "Link copied",
          message: "The current workspace and node deep link is on the clipboard.",
        });
      },
    },
  ];
}

export const addToSceneIcon = Binary;

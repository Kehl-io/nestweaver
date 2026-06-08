import type { ComponentType } from "react";
import {
  Binary,
  Bot,
  Compass,
  FileCode,
  GitCompare,
  GitFork,
  Network,
  Route,
  Search,
} from "lucide-react";
import { api } from "../../api/client";
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
  | "ask";

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
  focus?: DetailFocus;
  run: () => void | Promise<void>;
}

const SYMBOL_KINDS = new Set([
  "symbol",
  "Function",
  "Class",
  "Method",
  "Interface",
  "Trait",
  "Enum",
  "Module",
  "Extension",
  "Constant",
  "Property",
  "TypeAlias",
  "Variable",
]);

function isSymbolLike(kind?: string | null): boolean {
  return kind != null && SYMBOL_KINDS.has(kind);
}

function isNoteLike(uid: string, kind?: string | null): boolean {
  return kind === "note" || kind === "Note" || uid.startsWith("note:");
}

function nodeLabel(node: NodeActionContext): string {
  return node.label ?? node.uid.split(":").pop() ?? node.uid;
}

export function useNodeActions(node: NodeActionContext | null): NodeAction[] {
  const selectNode = useStore((s) => s.selectNode);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setDetailFocus = useStore((s) => s.setDetailFocus);
  const startPathfinding = useStore((s) => s.startPathfinding);
  const setFlowTrace = useStore((s) => s.setFlowTrace);
  const startDiff = useStore((s) => s.startDiff);
  const openLlmBar = useStore((s) => s.openLlmBar);
  const setLlmQuery = useStore((s) => s.setLlmQuery);

  if (!node) return [];

  const symbol = isSymbolLike(node.kind);
  const note = isNoteLike(node.uid, node.kind);
  const label = nodeLabel(node);

  const focusDetail = (focus: DetailFocus) => {
    selectNode(node.uid, node.kind ?? null);
    setDetailFocus(focus);
  };

  return [
    {
      id: "open",
      label: note ? "Open note" : symbol ? "Open source" : "Open detail",
      title: "Jump to the source, note, or detail preview for this item",
      icon: FileCode,
      focus: "source",
      run: () => focusDetail("source"),
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
      },
    },
    {
      id: "impact",
      label: "Impact",
      title: symbol ? "Show dependents and blast radius" : "Impact is available for symbols",
      icon: Network,
      disabled: !symbol,
      focus: "analysis",
      run: () => {
        if (!symbol) return;
        selectNode(node.uid, node.kind ?? null);
        setGraphMode("impact");
        setDetailFocus("analysis");
      },
    },
    {
      id: "related",
      label: "Related",
      title: "Jump to related code, references, backlinks, or mentions",
      icon: Search,
      focus: "related",
      run: () => focusDetail("related"),
    },
    {
      id: "path",
      label: "Path",
      title: "Find a path from this item to another node",
      icon: Route,
      focus: "analysis",
      run: () => {
        selectNode(node.uid, node.kind ?? null);
        startPathfinding(node.uid);
        setDetailFocus("analysis");
      },
    },
    {
      id: "compare",
      label: "Compare",
      title: "Compare this context with another seed set",
      icon: GitCompare,
      focus: "analysis",
      run: async () => {
        selectNode(node.uid, node.kind ?? null);
        setDetailFocus("analysis");
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
      focus: "analysis",
      run: async () => {
        if (!symbol) return;
        selectNode(node.uid, node.kind ?? null);
        setDetailFocus("analysis");
        const result = await api.flow(node.uid, 10);
        setFlowTrace(result as any);
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
        openLlmBar();
      },
    },
  ];
}

export const addToSceneIcon = Binary;
